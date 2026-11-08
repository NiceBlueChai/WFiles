//! 无头 UI 集成测试：真的开一个窗口、真的派发按键，
//! 断言命令面板从打开到执行完整跑通，而不只是“代码看起来对”。
//!
//! 放在 `#[cfg(test)]` 模块里而不是 `tests/` 目录：本项目是二进制 crate，
//! 集成测试目录链接不到 `WFilesApp`。

use gpui_kit::point;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions};
use gpui_kit::{Pixels, base::Root, px, size};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::app::{Starting, WFilesApp};
use crate::model::{Sort, SortKey, ViewMode};
use crate::session::Session;
use crate::settings::Settings;
use crate::terminal;

/// 开一个装着 WFilesApp 的测试窗口。
///
/// 见 [`open_app_of_width`]：放开"别的线程来唤醒"那一刀在它里面，
/// 两个入口共用同一条路，不再有谁漏掉。
fn open_app(cx: &mut TestAppContext) -> gpui_kit::WindowHandle<Root> {
    open_app_of_width(cx, 1180.).0
}

/// 指定窗口宽度开一个：网格的列数取决于窗格有多宽，要测就得能改它。
///
/// 窗格一打开就开始监听目录，而 notify 的回调线程随时可能把事件送回来
/// 唤醒任务——测试调度器把"别的线程来唤醒"判成不确定行为并直接报错。
/// 这不是被测代码的问题：监听本来就跨线程。所以开了窗口就统一放开，
/// 放在这里而不是 `open_app` 里，免得哪个测试直接调这一个入口时漏掉。
///
/// 连 WFilesApp 那一实体一起给出来：有些断言读状态比读界面准
/// （比如输入框里此刻填着什么，界面上只能看到占位提示）。
fn open_app_of_width(
    cx: &mut TestAppContext,
    width: f32,
) -> (gpui_kit::WindowHandle<Root>, gpui_kit::Entity<WFilesApp>) {
    open_app_at(cx, width, None)
}

/// 默认窗口高度。
///
/// 布局验收大多只关心宽度（列数、工具栏折叠），高度就取一个"够高"的值；
/// **侧栏那种竖向溢出**反过来只关心高度，得能自己指定——见
/// [`open_app_of_size`]。
const TEST_HEIGHT: f32 = 760.;

/// 指定宽高开一个：竖向装不下时才需要它。
///
/// 只改宽度的话，"侧栏内容长过窗口"这一类缺陷永远复现不出来——
/// 1180 × 760 的窗口比大多数机器的侧栏内容还高。
fn open_app_of_size(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
) -> (gpui_kit::WindowHandle<Root>, gpui_kit::Entity<WFilesApp>) {
    open_app_sized_starting(cx, width, height, Starting::primary(None))
}

/// 带着命令行目标开一个窗口：验"启动时落到哪儿"那几条路。
///
/// 目标从外面喂进来而不是真去读 `std::env::args`：命令行解析那一层
/// 在 `cli` 里有自己的测试，这里只验**落点**——两件事分开钉。
fn open_app_at(
    cx: &mut TestAppContext,
    width: f32,
    launch: Option<crate::cli::Launch>,
) -> (gpui_kit::WindowHandle<Root>, gpui_kit::Entity<WFilesApp>) {
    open_app_starting(cx, width, Starting::primary(launch))
}

/// 带着整份"这一次开窗的来路"开一个窗口：副窗（[`Starting::primary`]
/// 之外的两种身份）只有这里开得出来。
fn open_app_starting(
    cx: &mut TestAppContext,
    width: f32,
    starting: Starting,
) -> (gpui_kit::WindowHandle<Root>, gpui_kit::Entity<WFilesApp>) {
    open_app_sized_starting(cx, width, TEST_HEIGHT, starting)
}

/// 真正的开窗动作：宽、高、来路都由调用方定。
fn open_app_sized_starting(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
    starting: Starting,
) -> (gpui_kit::WindowHandle<Root>, gpui_kit::Entity<WFilesApp>) {
    cx.background_executor.allow_parking();
    // 每个测试的状态目录隔开。放在这里而不是让每个测试自己记得调：
    // 原来只有 22 / 75 个测试调了 `isolate_session`，剩下的会读写用户
    // 真实的 `~/Library/Application Support/WFiles/`——只要用户用过一次
    // 应用，那些测试的断言就可能被真机状态带偏（并跑时才挂，最难查）。
    ensure_isolated();
    // 上一条测试留在盘上的界面语言清掉（理由见 [`clear_leftover_locale`]）：
    // 必须赶在 `WFilesApp::new` 读设置**之前**，开完窗再改就晚了。
    clear_leftover_locale();
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::app::init(cx);
        crate::pane::init(cx);

        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
        };
        let (window, app) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| WFilesApp::new(window, cx, starting)),
        )
        .expect("打开测试窗口");
        (window.downcast::<Root>().expect("Base Root"), app)
    })
}

/// 走一步：一个完整的外层 `App::update`。
///
/// GPUI 只在**最外层** update 结束、即将返回时才 `flush_effects`，
/// 而 `cx.emit` 出来的事件就是一条 effect——所以输入框的 `InputEvent::Change`
/// 要等这一步走完才会送到订阅者手里。断言必须放在**下一步**里，
/// 不能跟触发它的按键挤在同一帧。真实应用每个事件各自是一个外层 update，
/// 不会看到这个延迟。
fn step(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App),
) {
    cx.update_window(handle.into(), |_, window, cx| f(window, cx))
        .expect("窗口还在");
}

/// 与 [`step`] 一样推一帧，但把回调的结果带回来（断言里要读界面上的字时用）。
fn step_value<T>(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> T,
) -> T {
    cx.update_window(handle.into(), |_, window, cx| f(window, cx))
        .expect("窗口还在")
}

thread_local! {
    /// 这一趟真正的隔离现场：锁 + 临时目录。
    ///
    /// 存起来是因为返回值装不下它（`open_app_of_width` 的调用方只要窗口和
    /// 实体），而它必须活到测试结束——临时目录一被删，应用后续的写盘就没
    /// 地方去了。测试跑在它自己的线程上，线程一结束 thread-local 就析构，
    /// 于是锁也刚好在这一刻放开。
    static OWNED: std::cell::RefCell<Option<Owned>> = const { std::cell::RefCell::new(None) };
}

struct Owned {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

/// 这一趟已经隔离好的状态目录；没隔离过就现在隔离。
///
/// **可重入**是关键：`WFILES_SESSION_DIR` 背后的锁是进程级的，
/// 同一个线程拿第二次就是自己等自己（死锁）。而"先隔离好、写完会话，
/// 再开窗口"是这个文件里最常见的测试写法。
///
/// `open_app_of_width` 里也调它，所以**没手动隔离的测试自动获得隔离**
/// ——原来只有 22 / 75 个测试记得调 `isolate_session`，剩下的会读写用户
/// 真实的 `~/Library/Application Support/WFiles/`。只要用户用过一次应用，
/// 那些测试的断言就可能被真机状态带偏（并跑时才挂，最难查）。
fn ensure_isolated() {
    if OWNED.with(|slot| slot.borrow().is_some()) {
        return;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    // 上一个持锁的线程 panic 过就把锁抢回来，别把整轮测试卡死。
    let guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("临时目录");
    unsafe { std::env::set_var("WFILES_SESSION_DIR", dir.path()) };
    // 语言环境一并钉死。界面语言在「跟随系统」那一档是从 `LC_ALL` /
    // `LC_MESSAGES` / `LANG` 推出来的（`i18n::Locale::from_env`），而这三个
    // 变量跟着**跑测试的 shell** 走：同一份代码在 `LANG=en_US.UTF-8` 的机器上
    // 整批变成英文界面、中文文案的断言全红，换个 shell 又全绿（本机实测：
    // 单跑 `the_pane_status_bar_speaks_english` / 那条命令面板切语言的测试
    // 必挂）。产品读环境是对的；测试要的是确定，所以这里把测试进程当成一台
    // 「系统语言就是中文」的机器。要验英文的那十几条自己改设置——设置排在
    // 环境前面，不受这里影响。
    unsafe {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            std::env::set_var(key, "zh_CN.UTF-8");
        }
    }
    OWNED.with(|slot| {
        slot.replace(Some(Owned {
            _guard: guard,
            _dir: dir,
        }))
    });
}

/// 把上一条测试留在隔离目录里的界面语言清成"没选过"（跟随系统）。
///
/// 语言是**当场落盘**的设置（`Settings::update` 里就 `save`），而隔离目录按
/// worker 线程复用：改英文的那十几条测试（都是先开窗、再改语言）会把 `en`
/// 留在 `settings.json` 里，同线程下一条测试开窗时被当成"用户选过英文"
/// 读回来——探针实测：盘上留 `en`，侧栏读出来就是 `Local file system`。
///
/// **要赶在应用构造之前**（[`open_app_starting`] 里挂的就是这个时机）：
/// `WFilesApp::new` 一上来就读这份设置，而且状态栏那类文案是*在事件里算好、
/// 缓存进 `self.status`* 的（见 `pane::update_status`）——开完窗再改语言已经
/// 晚了，第一条断言看到的仍是上一份设置算出来的英文。
///
/// 只清语言那一格：别的字段可能是这条测试自己种下的（记住的视图、标记、
/// 布局…），动它们等于把测试现场也清了。盘上没有设置文件、或者语言本来
/// 就是空的，一个字节都不写；要验英文的那几条在开窗**之后**自己写。
fn clear_leftover_locale() {
    let Some(path) = crate::settings::Settings::file_path() else {
        return;
    };
    let Some(mut stored) = crate::settings::Settings::load_from(&path) else {
        return;
    };
    if stored.locale.is_empty() {
        return;
    }
    stored.locale = String::new();
    let _ = stored.save_to(&path);
}

/// 隔离状态目录，并把路径交给调用方（用来往里写 session.json）。
///
/// 锁与临时目录只有一份、存在 thread-local 里活到测试结束，所以这里
/// 返回的只是个记路径的轻量句柄，掉不掉都不影响隔离。**同线程可重入**：
/// 已经在隔离里就直接给同一个路径，不去拿第二次锁。
fn isolate_session() -> SessionIsolation {
    ensure_isolated();
    let path = OWNED.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("刚隔离过")
            ._dir
            .path()
            .to_path_buf()
    });
    SessionIsolation { dir: path }
}

/// 会话与设置隔到临时目录，免得测试动到用户真实的标签恢复状态。
///
/// 见 [`ensure_isolated`]：这里只是它那层"顺手把路径给你"的壳。
struct SessionIsolation {
    dir: PathBuf,
}

impl SessionIsolation {
    fn path(&self) -> &Path {
        &self.dir
    }
}

/// 画两帧：滚动容器里的内容要第二帧才完成布局并注册进观察表，
/// 单帧之后 `try_find` 会漏掉刚出现的子元素。
fn settle(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    window.render_frame(cx);
    window.render_frame(cx);
}

/// 把侧栏滚到露出下半截（云盘 / 远程那几段）。
///
/// 侧栏中间那几段是**真滚动**的容器：内容高过窗口时多出来的部分滚出来，
/// 而不是溢出到窗口外面（页脚「本地文件系统」也不会被顶出去）。
/// 代价是折叠以下那几行在无头窗口里确实看不见——想点它们就得像真用户
/// 那样先滚一下，别指望 `click` 自己会滚。
fn scroll_sidebar(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    window.scroll(
        "sidebar-scroll",
        gpui_kit::ScrollDelta::Lines(point(0., -8.)),
        cx,
    );
    settle(window, cx);
}

/// 等一个后台任务的产物出现。
///
/// 缩略图是在真正的 OS 线程上解码的，不在 GPUI 的后台执行器里，
/// 所以只能给它一点真实时间；同时每一轮都得画帧——
/// 结果是通过事件送回来的，不画帧事件就不会被消费。
fn wait_for(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>, id: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // 把已经就绪的任务（解码线程送回来的那条消息）推到实体手里。
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window.try_find(id.to_string()).is_some()
            })
            .expect("窗口还在");
        if found || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 让应用打开指定目录：往隔离出来的会话目录里写一份 session.json。
///
/// 环境变量只挪得动状态目录，决定不了"从哪个目录开始"；
/// 而测试要在自己造的文件上按空格，所以走会话恢复这条路。
///
/// 隔离句柄要交还给调用方并活到测试结束：它一被丢弃临时目录就没了。
fn open_in(
    cx: &mut TestAppContext,
    dir: &Path,
) -> (SessionIsolation, gpui_kit::WindowHandle<Root>) {
    open_session(
        cx,
        &Session {
            tabs: vec![dir.to_path_buf()],
            active: 0,
            split: None,
            focus_on_left: true,
            sync: false,
        },
    )
}

/// [`open_in`] 但把应用实体一起交出来。
///
/// 地址栏那几条测试要读"这一栏现在在哪个目录、选中了什么"——那是断言的
/// 真实数据源，比从界面外观上猜准（界面这一层能读到的只有坐标和无障碍名）。
fn open_in_with_app(
    cx: &mut TestAppContext,
    dir: &Path,
) -> (
    SessionIsolation,
    gpui_kit::WindowHandle<Root>,
    gpui_kit::Entity<WFilesApp>,
) {
    let isolation = isolate_session();
    Session {
        tabs: vec![dir.to_path_buf()],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&isolation.path().join("session.json"))
    .expect("写测试会话");
    let (handle, app) = open_app_of_width(cx, 1180.);
    (isolation, handle, app)
}

/// [`open_in`] 但指定窗口宽度：布局相关的测试必须走这个。
///
/// 布局测试一定要用这个而不是 `open_in`——`open_in` 内部调的是
/// `open_app`，那里宽度写死 1180。拿它去"测四种宽度"的话，四次量到的
/// 是同一个窗口，测试全绿而缺陷照旧。
fn open_in_at_width(
    cx: &mut TestAppContext,
    dir: &Path,
    width: f32,
) -> (SessionIsolation, gpui_kit::WindowHandle<Root>) {
    let isolation = isolate_session();
    Session {
        tabs: vec![dir.to_path_buf()],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&isolation.path().join("session.json"))
    .expect("写测试会话");
    let handle = open_app_of_width(cx, width).0;
    (isolation, handle)
}

/// 按给定的会话启动应用；双栏、同步这些状态只能这么带进去。
fn open_session(
    cx: &mut TestAppContext,
    session: &Session,
) -> (SessionIsolation, gpui_kit::WindowHandle<Root>) {
    let isolation = isolate_session();
    session
        .save_to(&isolation.path().join("session.json"))
        .expect("写测试会话");
    let handle = open_app(cx);
    (isolation, handle)
}

/// [`open_session`] 但指定窗口宽度。
///
/// 同 [`open_in_at_width`]：`open_app` 里宽度写死 1180，拿它去"测三种窗口
/// 宽度"，三次量到的是同一个窗口——测试全绿而缺陷照旧。
fn open_session_at_width(
    cx: &mut TestAppContext,
    session: &Session,
    width: f32,
) -> (SessionIsolation, gpui_kit::WindowHandle<Root>) {
    let isolation = isolate_session();
    session
        .save_to(&isolation.path().join("session.json"))
        .expect("写测试会话");
    let handle = open_app_of_width(cx, width).0;
    (isolation, handle)
}

/// 某一栏里前几行的名字；两栏同屏时元素 id 是重复的，必须带范围查。
fn row_names_in(window: &mut gpui_kit::Window, slot: usize) -> Vec<String> {
    let mut names = Vec::new();
    {
        let scope = window.within(("pane-slot", slot));
        for position in 0usize..16 {
            let Some(row) = scope.try_find(("row-name", position)) else {
                break;
            };
            names.push(row.label().unwrap_or_default().to_string());
        }
    }
    names
}

/// 弹出菜单里第 `index` 项的标签文字；菜单没开或没有这一项返回空串。
///
/// 菜单项的 element id 就是序号（`Integer(index)`），没有按文字查的接口。
/// 而序号会随菜单内容漂移——加一项、去掉一个分隔符，后面全部错位。
/// 所以测试里要**先读出这一项叫什么、确认是自己要找的那条，再点它**，
/// 别直接拿一个写死的序号去点。
fn menu_item_label(window: &mut gpui_kit::Window, index: usize) -> String {
    window
        .within("popup-menu")
        .try_find(index)
        .and_then(|item| item.label().map(|label| label.to_string()))
        .unwrap_or_default()
}

/// 某一栏标的是"焦点栏"还是"另一栏"。
fn slot_label(window: &mut gpui_kit::Window, slot: usize) -> String {
    window
        .try_find(("pane-slot", slot))
        .map(|row| row.label().unwrap_or_default().to_string())
        .unwrap_or_default()
}

/// 某一栏的盒子（双栏才有 `pane-slot` 那层壳；单栏直接是窗格自己）。
fn pane_box(window: &mut gpui_kit::Window, slot: usize) -> Bounds<Pixels> {
    window
        .try_find(("pane-slot", slot))
        .expect("量布局先要有栏位（单栏时没有 pane-slot 这层壳，双栏才有）")
        .bounds()
}

/// 某一栏的右边缘。
fn pane_right(window: &mut gpui_kit::Window, slot: usize) -> Pixels {
    let bounds = pane_box(window, slot);
    bounds.origin.x + bounds.size.width
}

/// 某个元素在指定那一栏里的盒子；**没画**（被降级掉了）才是 `None`。
///
/// 裁剪不影响它：GPUI 只裁绘制，元素照样在树里、量得到——这正是溢出能被
/// 抓住的原因（内容画到栏外了，右边缘那条判据就红）。被降级的那些是
/// **整格不画**，所以查不到，两者分得开。
/// 两栏同屏时元素 id 是重复的，所以必须带栏号查（同 [`row_names_in`]）。
fn in_pane(
    window: &mut gpui_kit::Window,
    slot: usize,
    id: impl Into<gpui_kit::ElementId>,
) -> Option<Bounds<Pixels>> {
    window
        .within(("pane-slot", slot))
        .try_find(id)
        .map(|element| element.bounds())
}

/// 某个元素右边缘越过限位多少 px；这个元素没画（降级掉了）算 0。
fn right_over(
    window: &mut gpui_kit::Window,
    slot: usize,
    id: impl Into<gpui_kit::ElementId>,
    limit: Pixels,
) -> f32 {
    in_pane(window, slot, id)
        .map(|bounds| (bounds.origin.x + bounds.size.width - limit).as_f32())
        .unwrap_or(0.)
}

/// 表头此刻**画了**哪几列（按 `("header", 序号)` 在不在）。
///
/// [`ListColumns`] 的降级是"整格不画"，所以"在不在"就是降级判定本身。
/// 画了但被栏裁掉的元素**仍然算"画了"**——那说明该降级的地方没降级，
/// 这一条会红（红检时正是这样抓到的）。
fn header_columns(window: &mut gpui_kit::Window, slot: usize) -> Vec<usize> {
    let scope = window.within(("pane-slot", slot));
    (0usize..4)
        .filter(|&index| scope.try_find(("header", index)).is_some())
        .collect()
}

/// 详情板里那个文件名；没有选中项时是空串。
fn detail_name(window: &mut gpui_kit::Window) -> String {
    window
        .try_find("detail-name")
        .and_then(|element| element.label().map(|label| label.to_string()))
        .unwrap_or_default()
}

#[gpui_kit::test]
fn command_palette_opens_filters_and_runs(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    // 启动时不该自己弹面板。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "启动时不该弹命令面板"
        );
    });

    // Cmd+P 弹出面板，空查询列出全部可用命令。
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_some(),
            "Cmd+P 应该弹出命令面板"
        );
        assert!(
            window.try_find("pane:Refresh").is_some(),
            "空查询应该列出全部可用命令"
        );
    });

    // 输入收窄到只剩“新建标签页”。
    step(cx, handle, |window, cx| {
        window.input("新建标签页", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let row = window
            .try_find("app:new-tab")
            .expect("按标题应该能搜到新建标签页");
        assert_eq!(row.label(), Some("新建标签页"));
        assert!(
            window.try_find("pane:Refresh").is_none(),
            "不相关的命令应该被过滤掉"
        );
    });

    // Enter 执行：面板关上，并且真的多出一个标签。
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "执行后面板应该关闭"
        );
        assert!(
            window.try_find(("tab", 1usize)).is_some(),
            "命令面板应该真的新建了第二个标签"
        );
    });

    // 再开一次，Esc 关掉。
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_some(),
            "再按一次 Cmd+P 应该重新打开"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "Esc 应该关闭命令面板"
        );
    });
}

#[gpui_kit::test]
fn command_palette_sets_and_persists_the_default_terminal(cx: &mut TestAppContext) {
    let session = isolate_session();
    let handle = open_app(cx);

    // 装了的终端里挑第一个；一个都没装时退到“跟随系统”，照样能验落盘。
    let expected = terminal::candidates()
        .into_iter()
        .find(terminal::installed)
        .map(|option| option.id.to_string())
        .unwrap_or_else(|| terminal::AUTO.to_string());

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("终端", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let row = window
            .try_find(format!("terminal:{expected}"))
            .expect("面板里应该列出这个终端");
        let label = row.label().unwrap_or_default();
        assert!(
            label.starts_with("默认终端："),
            "终端条目应该长成“默认终端：X”，实际是 {label}"
        );
    });

    // 点一下就改设置：面板关上，并且真的写进了 settings.json。
    let row_id = format!("terminal:{expected}");
    step(cx, handle, move |window, cx| {
        window.click(row_id, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "改完设置后面板应该关上"
        );
    });

    let saved =
        Settings::load_from(&session.path().join("settings.json")).expect("设置应该已经落盘");
    assert_eq!(saved.terminal, expected);
}

/// 当前列表里前几行的名字，用来断言排序结果。
fn row_names(window: &mut gpui_kit::Window) -> Vec<String> {
    let mut names = Vec::new();
    // 下标必须是 usize：ElementId 只认 `(&str, usize)`，
    // 让编译器自己推断会落到 i32 上。
    for position in 0usize..16 {
        let Some(row) = window.try_find(("row-name", position)) else {
            break;
        };
        names.push(row.label().unwrap_or_default().to_string());
    }
    names
}

#[gpui_kit::test]
fn a_folder_keeps_its_sort_between_visits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // 名字顺序和大小顺序故意相反：这样"记住了排序"和"默认按名字"一眼能分开。
    for (name, length) in [("a.txt", 300usize), ("b.txt", 100), ("c.txt", 200)] {
        std::fs::write(dir.path().join(name), "x".repeat(length)).unwrap();
    }
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            row_names(window),
            ["a.txt", "b.txt", "c.txt"],
            "默认是按名字"
        );
    });

    // 点"大小"表头，改成按大小升序。
    step(cx, handle, |window, cx| {
        window.click(("header", 1usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names(window), ["b.txt", "c.txt", "a.txt"]);
    });

    // 这个目录的排序应该已经写进设置了。
    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置应该落盘");
    assert_eq!(
        saved.view_for(dir.path()).map(|view| view.sort),
        Some(Sort {
            key: SortKey::Size,
            descending: false
        })
    );
}

#[gpui_kit::test]
fn a_remembered_sort_is_applied_when_the_folder_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for (name, length) in [("a.txt", 300usize), ("b.txt", 100), ("c.txt", 200)] {
        std::fs::write(dir.path().join(name), "x".repeat(length)).unwrap();
    }

    // 预先埋一份"上次来过，按大小降序"的设置，再看打开时有没有套用。
    let session = isolate_session();
    let mut seeded = Settings::default();
    seeded.remember_view(
        dir.path().to_path_buf(),
        Sort {
            key: SortKey::Size,
            descending: true,
        },
        ViewMode::default(),
    );
    seeded
        .save_to(&session.path().join("settings.json"))
        .expect("写测试设置");
    Session {
        tabs: vec![dir.path().to_path_buf()],
        active: 0,
        sync: false,
        focus_on_left: true,
        split: None,
    }
    .save_to(&session.path().join("session.json"))
    .expect("写测试会话");
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            row_names(window),
            ["a.txt", "c.txt", "b.txt"],
            "打开这个目录时应该套用记住的排序"
        );
    });
}

#[gpui_kit::test]
fn the_window_layout_is_remembered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (session, handle) = open_in(cx, dir.path());

    // 渲染第一帧就会记下窗口位置尺寸。
    step(cx, handle, |window, cx| {
        settle(window, cx);
    });

    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置应该落盘");
    let layout = saved.layout.expect("应该记下了窗口布局");
    assert!(layout.usable(), "记下来的这份得真能用：{layout:?}");
    assert!(
        (layout.width - 1180.).abs() < 1. && (layout.height - 760.).abs() < 1.,
        "测试窗口是 1180×760，实际记下的是 {layout:?}"
    );
}

#[gpui_kit::test]
fn quick_look_shows_a_text_file_and_closes_on_escape(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "第一行\n第二行\n第三行\n").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("quick-look").is_none(),
            "启动时不该有预览浮层"
        );
    });

    // 选中那一个文件，再按空格。
    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("space", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("quick-look").is_some(),
            "空格应该打开 Quick Look"
        );
        assert!(
            window.try_find("quick-look-text").is_some(),
            "文本文件应该走文本预览，而不是十六进制"
        );
        let row = window
            .try_find("quick-look-subtitle")
            .expect("应该有副标题");
        let subtitle = row.label().unwrap_or_default();
        assert_eq!(subtitle, "文本 · 3 行", "副标题要报出真实行数：{subtitle}");
    });

    // Esc 关掉，而不是去清空选中。
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("quick-look").is_none(),
            "Esc 应该先关掉预览浮层"
        );
    });
}

#[gpui_kit::test]
fn quick_look_decodes_an_image_in_the_background(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pic.png");
    image::RgbImage::from_fn(80, 40, |x, y| image::Rgb([x as u8, y as u8, 90]))
        .save(&path)
        .unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("space", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("quick-look").is_some(),
            "空格应该打开预览浮层"
        );
    });

    // 解码在别的线程上，等它回来。
    assert!(
        wait_for(cx, handle, "quick-look-image"),
        "后台解码完之后应该换成真图"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let row = window
            .try_find("quick-look-subtitle")
            .expect("应该有副标题");
        let subtitle = row.label().unwrap_or_default();
        assert!(
            subtitle.contains("80 × 40"),
            "副标题要报出原图尺寸，实际是 {subtitle}"
        );
    });
}

#[gpui_kit::test]
fn quick_look_dumps_a_binary_file_as_hex(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // 带 NUL 的一小段二进制：既验证嗅探，也验证转储排版。
    std::fs::write(dir.path().join("blob.bin"), b"\x00\x01Hello\xff\xfe").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("space", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("quick-look-hex").is_some(),
            "二进制应该转储成十六进制"
        );
        assert!(
            window.try_find("quick-look-text").is_none(),
            "有 NUL 字节就不该当文本显示"
        );
        let row = window
            .try_find("quick-look-subtitle")
            .expect("应该有副标题");
        let subtitle = row.label().unwrap_or_default();
        assert!(subtitle.contains("二进制"), "实际是 {subtitle}");
    });
}

#[gpui_kit::test]
fn command_palette_hides_unavailable_commands(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);

        // 跟选中无关的一直在。
        assert!(window.try_find("pane:Refresh").is_some());
        assert!(window.try_find("pane:NewFolder").is_some());

        // 没有任何选中项时，重命名/批量重命名/回收站都不该出现。
        assert!(window.try_find("pane:Rename").is_none());
        assert!(window.try_find("pane:BulkRename").is_none());
        assert!(window.try_find("pane:Trash").is_none());
        // 本地目录里没有"远程文件"这回事，回传那条命令不该列出来。
        assert!(window.try_find("pane:EditRemote").is_none());
    });

    // 就算主动去搜，也搜不出来。
    step(cx, handle, |window, cx| {
        window.input("重命名", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("pane:Rename").is_none(),
            "没选中文件时不该列出重命名类命令"
        );
        assert!(
            window.try_find("pane:Refresh").is_none(),
            "查询词不命中时不该留下别的东西"
        );
    });
}

// ---- 后台操作中心 ----

use crate::ops_entity::Ops;

/// 一个"拷起来要花点时间"的源目录。
///
/// 单个大文件在这台机器上一眨眼就拷完了（64 MB 不到 10 毫秒），
/// 根本抓不到"拷到一半"这个瞬间。所以靠**数量**而不是体积来拖时间：
/// 三千个文件就是三千次 open/write/close，稳在几百毫秒的量级。
const FILE_COUNT: usize = 3_000;
const FILE_SIZE: u64 = 4096;

fn many_files(dir: &Path) -> u64 {
    std::fs::create_dir_all(dir).unwrap();
    for ix in 0..FILE_COUNT {
        std::fs::write(
            dir.join(format!("f{ix}.bin")),
            vec![ix as u8; FILE_SIZE as usize],
        )
        .unwrap();
    }
    FILE_COUNT as u64 * FILE_SIZE
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.count())
        .unwrap_or(0)
}

fn new_ops(cx: &mut TestAppContext) -> gpui_kit::Entity<Ops> {
    // 后台线程会持续把进度送回来，唤醒等在 `recv()` 上的那个任务。
    // GPUI 的测试调度器把"别的线程来唤醒"判成不确定行为并直接报错，
    // 所以这几条测试要显式开 parking：它们验的本来就是真实的线程协作。
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.new(|_| Ops::new(None))
    })
}

/// 等后台任务把进度推上来，返回当前已拷贝的字节数。
///
/// 拷贝跑在真的 OS 线程上，只能给它真实时间；每轮都要
/// `run_until_parked` —— 消息是经后台执行器送进实体的。
/// 任务已经跑完时读到的是 0，调用方据此判断"没抓到中途"。
fn wait_for_bytes(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>, at_least: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let done = cx.update(|cx| {
            ops.read(cx)
                .running()
                .map_or(0, |running| running.bytes_done)
        });
        if done >= at_least || Instant::now() > deadline {
            return done;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 此刻的已拷贝字节数（顺手把已就绪的消息推一次）。
fn bytes_now(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>) -> u64 {
    cx.run_until_parked();
    cx.update(|cx| {
        ops.read(cx)
            .running()
            .map_or(0, |running| running.bytes_done)
    })
}

/// 等队列彻底排空：没在跑的，也没有排着的。
fn wait_until_idle(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let idle = cx.update(|cx| {
            let ops = ops.read(cx);
            ops.running().is_none() && ops.queued() == 0
        });
        if idle || Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 状态栏那句话（中文形态）。
///
/// **用 `to_string()` 不是 `say(cx)`**：`Display` 恒给中文，
/// 所以这个读法跟语言无关——切到英文的测试另有
/// `the_status_bar_speaks_english` 那类用例在管。
fn message_of(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>) -> String {
    cx.update(|cx| {
        ops.read(cx)
            .last_message()
            .map(ToString::to_string)
            .unwrap_or_default()
    })
}

#[gpui_kit::test]
fn a_copy_can_be_paused_and_resumed(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    let total = many_files(&src);
    let dest = root.path().join("dest");
    std::fs::create_dir(&dest).unwrap();

    let ops = new_ops(cx);
    cx.update(|cx| ops.update(cx, |ops, cx| ops.copy(vec![src.clone()], cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.request_paste(dest.clone(), cx)));

    let in_flight = wait_for_bytes(cx, &ops, 256 * 1024);
    assert!(
        in_flight > 0 && in_flight < total,
        "要抓在拷贝到一半的时候暂停，实际停在 {in_flight}/{total}"
    );

    cx.update(|cx| ops.update(cx, |ops, cx| ops.pause(cx)));
    // 暂停是在下一个数据块边界才生效的，之前发出去的几条进度还在路上，
    // 得先给它们落地的时间再取基准值，否则比的是"消息送达的快慢"。
    std::thread::sleep(Duration::from_millis(80));
    let at_pause = bytes_now(cx, &ops);
    std::thread::sleep(Duration::from_millis(200));
    let still = bytes_now(cx, &ops);
    assert_eq!(still, at_pause, "暂停之后 200 毫秒不该再往前走");
    assert!(
        cx.update(|cx| {
            ops.read(cx)
                .running()
                .is_some_and(|running| running.state == crate::ops::ControlState::Paused)
        }),
        "状态该标成已暂停"
    );

    cx.update(|cx| ops.update(cx, |ops, cx| ops.resume(cx)));
    wait_until_idle(cx, &ops);

    assert_eq!(file_count(&dest.join("src")), FILE_COUNT, "恢复之后该拷完");
    assert!(
        message_of(cx, &ops).contains("拷贝完成 1 项"),
        "实际是 {}",
        message_of(cx, &ops)
    );
}

#[gpui_kit::test]
fn an_aborted_copy_leaves_no_truncated_file(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    many_files(&src);
    let dest = root.path().join("dest");
    std::fs::create_dir(&dest).unwrap();

    let ops = new_ops(cx);
    cx.update(|cx| ops.update(cx, |ops, cx| ops.copy(vec![src.clone()], cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.request_paste(dest.clone(), cx)));

    assert!(wait_for_bytes(cx, &ops, 256 * 1024) > 0, "应该已经开工");
    cx.update(|cx| ops.update(cx, |ops, cx| ops.abort(cx)));
    wait_until_idle(cx, &ops);

    let copied = dest.join("src");
    let count = file_count(&copied);
    assert!(count < FILE_COUNT, "中止之后不该拷完，实际拷了 {count} 个");
    // 拷了一半的那个会被删掉，留下的每一个都该是完整的。
    for entry in std::fs::read_dir(&copied).unwrap().flatten() {
        assert_eq!(
            entry.metadata().unwrap().len(),
            FILE_SIZE,
            "{} 被截断了",
            entry.file_name().to_string_lossy()
        );
    }
    assert!(
        message_of(cx, &ops).contains("已中止"),
        "实际是 {}",
        message_of(cx, &ops)
    );
}

#[gpui_kit::test]
fn a_second_transfer_waits_in_the_queue(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    many_files(&src);
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();

    let ops = new_ops(cx);
    cx.update(|cx| ops.update(cx, |ops, cx| ops.copy(vec![src.clone()], cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.request_paste(first.clone(), cx)));
    assert!(wait_for_bytes(cx, &ops, 256 * 1024) > 0, "第一个该开工");

    // 把第一个按住，第二个就只能排队——这是排队这条路径唯一确定的观测点。
    cx.update(|cx| ops.update(cx, |ops, cx| ops.pause(cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.copy(vec![src.clone()], cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.request_paste(second.clone(), cx)));

    assert_eq!(cx.update(|cx| ops.read(cx).queued()), 1, "第二个该排在队里");
    assert!(file_count(&second) == 0, "排在队里的不该提前开始拷贝");

    cx.update(|cx| ops.update(cx, |ops, cx| ops.resume(cx)));
    wait_until_idle(cx, &ops);

    assert_eq!(file_count(&first.join("src")), FILE_COUNT, "第一个");
    assert_eq!(file_count(&second.join("src")), FILE_COUNT, "第二个");
    assert_eq!(cx.update(|cx| ops.read(cx).queued()), 0, "队列该排空");
}

#[gpui_kit::test]
#[cfg(unix)]
fn a_failed_item_can_be_retried(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let file = src.join("locked.txt");
    std::fs::write(&file, "hi").unwrap();
    let dest = root.path().join("dest");
    std::fs::create_dir(&dest).unwrap();

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    // 以 root 跑的时候权限位挡不住读，这条就没得验了，直接跳过。
    if std::fs::read(&file).is_ok() {
        std::fs::remove_file(&file).ok();
        return;
    }

    let ops = new_ops(cx);
    cx.update(|cx| ops.update(cx, |ops, cx| ops.copy(vec![file.clone()], cx)));
    cx.update(|cx| ops.update(cx, |ops, cx| ops.request_paste(dest.clone(), cx)));
    wait_until_idle(cx, &ops);

    assert!(!dest.join("locked.txt").exists());
    assert_eq!(
        cx.update(|cx| ops.read(cx).retry_label().map(|it| it.to_string())),
        Some("重试 1 项".to_string())
    );

    // 把权限放回来再重试：这一次该过。
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    cx.update(|cx| ops.update(cx, |ops, cx| ops.retry(cx)));
    wait_until_idle(cx, &ops);

    assert_eq!(
        std::fs::read_to_string(dest.join("locked.txt")).expect("重试之后该拷过去"),
        "hi"
    );
    assert_eq!(
        cx.update(|cx| ops.read(cx).retry_label()),
        None,
        "成功了就不该再挂着重试按钮"
    );
}

/// 等某一栏里出现指定名字的行。
///
/// 后台任务的结论要经执行器推进实体、再画帧才看得见，
/// 所以每一轮都要 `run_until_parked` + `settle`。
fn wait_for_row(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    slot: usize,
    name: &str,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                row_names_in(window, slot)
                    .iter()
                    .any(|row| row.as_str() == name)
            })
            .expect("窗口还在");
        if found || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 等一个文件真的在磁盘上出现。拷贝在后台跑，得把执行器推到底。
fn wait_for_file(cx: &mut TestAppContext, path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if path.exists() || Instant::now() > deadline {
            return path.exists();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 等一个文件真的从磁盘上消失（移动走的那一半）。
fn wait_until_file_gone(cx: &mut TestAppContext, path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if !path.exists() || Instant::now() > deadline {
            return !path.exists();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 等到某个元素消失（任务跑完、面板关掉之类）。
fn wait_until_gone(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let gone = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window.try_find(id.to_string()).is_none()
            })
            .expect("窗口还在");
        if gone || Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui_kit::test]
fn the_status_bar_drives_a_running_transfer(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    many_files(&src);
    std::fs::create_dir(root.path().join("dest")).unwrap();

    let (_session, handle) = open_in(cx, root.path());
    // 后台线程要持续把进度送回来，见 [`new_ops`] 里的说明。
    cx.background_executor.allow_parking();
    // 这条测试要看的是**搬运还在跑**时的界面：进度行、中止按钮、暂停后的
    // 文案。而 12 MB（3000 个 4 KB）在这台机器上一闪就过去了——实测进度行
    // 4 ms 就出现，中止按钮那条断言在它出现与消失之间只有几十毫秒，
    // 全量跑时偶发挂在「还该有中止按钮」上。
    //
    // 限到 3 MB/s：实测这一趟稳定地跑六七秒，三样东西都在窗口里。量的是
    // 界面在搬运中的样子，不是这台机器有多快。
    cx.update(|cx| {
        Settings::update(cx, |settings| settings.speed_limit = 3_000_000);
    });

    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 按名字排序：dest 在前，src 在后。
        assert_eq!(row_names(window), vec!["dest", "src"]);
    });

    // 选中 src、复制，进到 dest 里粘贴。
    step(cx, handle, |window, cx| {
        window.click(("row-name", 1usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-c", cx);
    });
    step(cx, handle, |window, cx| {
        window.click(("row-name", 0usize), cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-v", cx);
    });

    assert!(
        wait_for(cx, handle, "status-detail"),
        "拷贝期间状态栏该显示进度"
    );
    assert!(wait_for(cx, handle, "stop"), "还该有中止按钮");

    // 点暂停：状态栏改口，进度停住。
    step(cx, handle, |window, cx| {
        window.click("transport", cx);
    });
    let paused_text = cx
        .update_window(handle.into(), |_, window, cx| {
            settle(window, cx);
            window
                .try_find("status-detail")
                .expect("暂停时任务还在")
                .label()
                .unwrap_or_default()
                .to_string()
        })
        .expect("窗口还在");
    assert!(paused_text.contains("已暂停"), "实际是 {paused_text}");

    // 点继续：跑完之后状态栏回到消息行。
    step(cx, handle, |window, cx| {
        window.click("transport", cx);
    });
    wait_until_gone(cx, handle, "status-detail");

    assert_eq!(
        file_count(&root.path().join("dest").join("src")),
        FILE_COUNT
    );
}

// ---- 双栏 ----

#[gpui_kit::test]
fn splitting_shows_a_second_pane(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    std::fs::write(root.path().join("a.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find(("pane-slot", 0usize)).is_none(),
            "默认是单栏"
        );
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-alt-d", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 只有一个标签时分栏会复制一份出来，两边先看同一个目录。
        assert_eq!(row_names_in(window, 0), vec!["a.txt"]);
        assert_eq!(row_names_in(window, 1), vec!["a.txt"]);
        assert_eq!(slot_label(window, 0), "焦点栏");
        assert_eq!(slot_label(window, 1), "另一栏");
    });

    // 再按一次回到单栏。
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-d", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find(("pane-slot", 1usize)).is_none(),
            "再按一次该回到单栏"
        );
    });
}

#[gpui_kit::test]
fn tab_moves_the_focus_without_swapping_the_panes(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();

    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left, right],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(slot_label(window, 0), "焦点栏");
        assert_eq!(slot_label(window, 1), "另一栏");
    });

    step(cx, handle, |window, cx| {
        window.press("tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(slot_label(window, 0), "另一栏", "焦点挪走了");
        assert_eq!(slot_label(window, 1), "焦点栏");
    });

    // 换焦点不该让两栏的位置也换掉：右边那栏还是 right。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(row_names_in(window, 1).is_empty(), "右栏看的是空目录");
    });
}

#[gpui_kit::test]
fn f5_copies_the_selection_into_the_other_pane(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    std::fs::write(left.join("report.txt"), "hi").unwrap();

    // 拷进来的文件会让目录监听回调醒来唤醒任务，
    // 测试调度器不允许别的线程这么干（见 [`new_ops`] 的说明）。
    cx.background_executor.allow_parking();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names_in(window, 0), vec!["report.txt"]);
        assert!(row_names_in(window, 1).is_empty());
    });

    // 选中左栏那一项，F5 拷到另一栏。
    step(cx, handle, |window, cx| {
        window
            .within(("pane-slot", 0usize))
            .click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("f5", cx);
    });

    assert!(
        wait_for_row(cx, handle, 1, "report.txt"),
        "另一栏该刷新出拷过来的文件"
    );
    assert!(
        right.join("report.txt").exists(),
        "F5 应该把选中项拷进另一栏正在看的目录"
    );
    assert!(left.join("report.txt").exists(), "拷贝不动源文件");
}

#[gpui_kit::test]
fn synced_panes_follow_each_other(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let inner = root.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(inner.join("only.txt"), "x").unwrap();

    // 两个标签都开在 root，并打开同步浏览。
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![root.path().to_path_buf(), root.path().to_path_buf()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: true,
        },
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names_in(window, 0), vec!["inner"]);
        assert_eq!(row_names_in(window, 1), vec!["inner"], "开启同步时两边一致");
    });

    // 左栏进到 inner，右栏该跟着走。
    step(cx, handle, |window, cx| {
        window
            .within(("pane-slot", 0usize))
            .click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names_in(window, 0), vec!["only.txt"], "左栏进去了");
        assert_eq!(row_names_in(window, 1), vec!["only.txt"], "右栏该跟着进去");
    });

    // 关掉同步之后，右栏自己走自己的。
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-s", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("backspace", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names_in(window, 0), vec!["inner"], "左栏退回去了");
        assert_eq!(row_names_in(window, 1), vec!["only.txt"], "右栏留在原地");
    });
}

// ---- 视图模式（列表 / 网格）----

/// 现在是哪种视图：三种视图的容器 id 各不相同，只看哪个在。
fn view_of(window: &mut gpui_kit::Window) -> &'static str {
    for (id, name) in [
        ("file-list", "列表"),
        ("file-grid", "网格"),
        ("columns", "分列"),
    ] {
        if window.try_find(id).is_some() {
            return name;
        }
    }
    "空"
}

/// 第一行上并排放了几个格子。
///
/// 拿它断言"窗口越宽一行放得越多"：网格的列数是从量出来的窗格宽度算的，
/// 而这个数字在界面上没有一个能直接读出来的地方。
fn tiles_in_first_row(window: &mut gpui_kit::Window) -> usize {
    let Some(first) = window.try_find(("tile", 0usize)) else {
        return 0;
    };
    let row_top = first.bounds().origin.y;
    (0usize..64)
        .map(|position| window.try_find(("tile", position)))
        .take_while(|tile| {
            tile.as_ref()
                .is_some_and(|tile| tile.bounds().origin.y == row_top)
        })
        .count()
}

#[gpui_kit::test]
fn switching_to_the_grid_shows_the_same_files(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.path().join(name), "x").unwrap();
    }
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "列表");
        assert_eq!(row_names(window), ["a.txt", "b.txt", "c.txt"]);
    });

    // 点工具栏那颗网格按钮。
    step(cx, handle, |window, cx| {
        window.click("view-grid", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "网格", "点完按钮该换成网格");
        assert!(
            window.try_find(("tile", 0usize)).is_some(),
            "网格里该有格子"
        );
        // 换视图只是换画法，里面还是同一批文件、同一个顺序。
        assert_eq!(row_names(window), ["a.txt", "b.txt", "c.txt"]);
    });

    // 再点回列表。
    step(cx, handle, |window, cx| {
        window.click("view-list", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "列表");
        assert!(
            window.try_find("header").is_none() && window.try_find(("header", 0usize)).is_some(),
            "回到列表该有表头"
        );
    });
}

#[gpui_kit::test]
fn cmd_1_and_cmd_2_switch_the_view(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "列表");
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "网格", "Cmd+2 切到网格");
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-1", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "列表", "Cmd+1 切回列表");
    });
}

#[gpui_kit::test]
fn the_grid_packs_several_tiles_into_one_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..12 {
        std::fs::write(dir.path().join(format!("f{index:02}.txt")), "x").unwrap();
    }
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.click("view-grid", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let columns = tiles_in_first_row(window);
        assert!(
            columns >= 2,
            "窗格这么宽，一行至少该放两个格子，实际 {columns}"
        );
        assert!(
            window.try_find(("tile", 11usize)).is_some(),
            "12 项都在，一个都不能少"
        );
        // 12 项装不进一行：最后一项必须落在更下面的一行。
        let first = window.find(("tile", 0usize)).bounds().origin;
        let last = window.find(("tile", 11usize)).bounds().origin;
        assert!(
            last.y > first.y,
            "第 12 项该换行到下面去：{first:?} {last:?}"
        );
    });
}

#[gpui_kit::test]
fn a_narrower_pane_gets_fewer_columns(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..12 {
        std::fs::write(dir.path().join(format!("f{index:02}.txt")), "x").unwrap();
    }

    // 同一个目录各开一次：宽窗口和窄窗口的一行列数必须不一样，
    // 否则说明"按窗格宽度算列数"根本没生效（比如宽度没量出来）。
    let wide = columns_at_width(cx, dir.path(), 1180.);
    let narrow = columns_at_width(cx, dir.path(), 620.);

    assert!(narrow >= 1, "窄窗口也得画出格子来，实际 {narrow} 列");
    assert!(
        wide > narrow,
        "窗口窄了，一行该少放几格：宽窗口 {wide} 列，窄窗口 {narrow} 列"
    );
}

/// 在指定宽度的窗口里打开这个目录、切到网格，返回第一行有几个格子。
///
/// 用 Cmd+2 而不是点工具栏按钮：窄窗口下工具栏本来就摆不开，
/// 按钮会被挤到看不见的地方，而按键不受布局影响。
fn columns_at_width(cx: &mut TestAppContext, dir: &Path, width: f32) -> usize {
    let session = isolate_session();
    Session {
        tabs: vec![dir.to_path_buf()],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&session.path().join("session.json"))
    .expect("写测试会话");
    let (handle, _app) = open_app_of_width(cx, width);

    let mut columns = 0;
    step(cx, handle, |window, cx| {
        window.press("cmd-2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        columns = tiles_in_first_row(window);
    });
    columns
}

#[gpui_kit::test]
fn a_folder_keeps_its_view_between_visits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.click("view-grid", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "网格");
    });

    // 视图和排序一样，是这个目录的属性：得跟着目录记住，不是跟着窗格。
    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置应该落盘");
    let remembered = saved.view_for(dir.path()).expect("这个目录该记住了");
    assert_eq!(remembered.view, ViewMode::Grid);
}

/// 分列视图里某一列的标题（就是那一列的目录名）。
fn column_title(window: &mut gpui_kit::Window, index: usize) -> Option<String> {
    window
        .try_find(("column", index))
        .map(|column| column.label().unwrap_or_default().to_string())
}

/// 分列视图里某一列前几行的名字。
fn column_rows(window: &mut gpui_kit::Window, index: usize) -> Vec<String> {
    let mut names = Vec::new();
    {
        let scope = window.within(("column", index));
        for position in 0usize..16 {
            let Some(row) = scope.try_find(("column-row", position)) else {
                break;
            };
            names.push(row.label().unwrap_or_default().to_string());
        }
    }
    names
}

#[gpui_kit::test]
fn the_column_view_shows_the_whole_path_one_level_per_column(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("inner")).unwrap();
    std::fs::write(dir.path().join("inner").join("deep.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, &dir.path().join("inner"));

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "列表");
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-3", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "分列", "Cmd+3 切到分列");

        assert!(
            window.try_find(("column", 0usize)).is_some(),
            "至少要有一列"
        );
        // 最后一列的内容就是当前目录。
        let mut last_index = 0usize;
        while window.try_find(("column", last_index + 1)).is_some() {
            last_index += 1;
        }
        assert_eq!(
            column_title(window, last_index).as_deref(),
            Some("inner"),
            "最后一列该是当前目录"
        );
        assert_eq!(column_rows(window, last_index), vec!["deep.txt"]);
    });
}

#[gpui_kit::test]
fn clicking_a_folder_in_an_earlier_column_navigates_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("one")).unwrap();
    std::fs::create_dir(dir.path().join("one").join("two")).unwrap();
    std::fs::write(dir.path().join("one").join("a.txt"), "x").unwrap();
    std::fs::write(dir.path().join("one").join("two").join("b.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, &dir.path().join("one").join("two"));

    step(cx, handle, |window, cx| {
        window.press("cmd-3", cx);
    });

    // 最后一列是 two，上一列是 one，再上一列是临时目录（里面只有 one）。
    let mut last = 0usize;
    step(cx, handle, |window, cx| {
        settle(window, cx);
        while window.try_find(("column", last + 1)).is_some() {
            last += 1;
        }
        assert_eq!(column_title(window, last).as_deref(), Some("two"));
        assert_eq!(column_rows(window, last), vec!["b.txt"]);
        let _ = cx;
    });

    // 点"临时目录"那一列里的 one：整条链在这里截断，one 成为当前目录。
    step(cx, handle, |window, cx| {
        let parent = last - 2;
        assert_eq!(
            column_rows(window, parent),
            vec!["one"],
            "临时目录里只有 one"
        );
        window
            .within(("column", parent))
            .click(("column-row", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let mut now = 0usize;
        while window.try_find(("column", now + 1)).is_some() {
            now += 1;
        }
        assert_eq!(now, last - 1, "少了一层：链在 one 这里截断");
        assert_eq!(column_title(window, now).as_deref(), Some("one"));
        // 文件夹排在前面，所以是 two 在前、a.txt 在后。
        assert_eq!(column_rows(window, now), ["two", "a.txt"]);
    });
}

#[gpui_kit::test]
fn the_keyboard_still_drives_the_last_column(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("one")).unwrap();
    std::fs::create_dir(dir.path().join("one").join("two")).unwrap();
    std::fs::write(dir.path().join("one").join("two").join("b.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, &dir.path().join("one"));

    step(cx, handle, |window, cx| {
        window.press("cmd-3", cx);
    });

    // 当前列是 one，里面只有 two 一个子目录。
    let mut last = 0usize;
    step(cx, handle, |window, cx| {
        settle(window, cx);
        while window.try_find(("column", last + 1)).is_some() {
            last += 1;
        }
        assert_eq!(column_title(window, last).as_deref(), Some("one"));
        assert_eq!(column_rows(window, last), vec!["two"]);
    });

    // 方向键把光标放上去，Enter 进去：键盘在分列视图里照旧管用。
    step(cx, handle, |window, cx| {
        window.press("down", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let mut now = 0usize;
        while window.try_find(("column", now + 1)).is_some() {
            now += 1;
        }
        assert_eq!(now, last + 1, "多铺了一层");
        assert_eq!(column_title(window, now).as_deref(), Some("two"));
        assert_eq!(column_rows(window, now), vec!["b.txt"]);
    });
}

#[gpui_kit::test]
fn a_remembered_view_is_applied_when_the_folder_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();

    let session = isolate_session();
    let mut seeded = Settings::default();
    seeded.remember_view(dir.path().to_path_buf(), Sort::default(), ViewMode::Grid);
    seeded
        .save_to(&session.path().join("settings.json"))
        .expect("写测试设置");
    Session {
        tabs: vec![dir.path().to_path_buf()],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&session.path().join("session.json"))
    .expect("写测试会话");
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(view_of(window), "网格", "打开这个目录时该套用记住的视图");
    });
}

/// 把一批"从系统拖进来"的文件落在屏幕上的 `position` 处。
///
/// 系统拖放（Finder、资源管理器）走的是 `FileDropEvent`：
/// 先 Entered 报"带着什么进来了"，再 Submit 报"松手了"。
fn drop_from_outside(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    position: Point<gpui_kit::Pixels>,
    paths: &[std::path::PathBuf],
) {
    step(cx, handle, |window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::FileDrop(gpui_kit::FileDropEvent::Entered {
                position,
                paths: gpui_kit::ExternalPaths(paths.iter().cloned().collect()),
            }),
            cx,
        );
    });
    step(cx, handle, |window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::FileDrop(gpui_kit::FileDropEvent::Submit { position }),
            cx,
        );
    });
}

/// 元素中心：投放坐标要落在元素里头，取中心最稳。
fn center_of(window: &mut gpui_kit::Window, id: gpui_kit::ElementId) -> Point<gpui_kit::Pixels> {
    let bounds = window.try_find(id).expect("找得到这个元素").bounds();
    gpui_kit::point(
        bounds.origin.x + bounds.size.width / 2.,
        bounds.origin.y + bounds.size.height / 2.,
    )
}

#[gpui_kit::test]
fn a_file_dropped_from_outside_lands_in_the_folder(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();

    // 拖进来的这个文件"属于系统"，我们无权删它，之后要断言它还在。
    let outside = tempfile::tempdir().unwrap();
    let dragged = outside.path().join("photo.png");
    std::fs::write(&dragged, "png").unwrap();

    let (_session, handle) = open_in(cx, dir.path());

    let mut target = Point::default();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(row_names(window), ["inbox"]);
        target = center_of(window, ("row-drop", 0usize).into());
    });

    drop_from_outside(cx, handle, target, std::slice::from_ref(&dragged));

    assert!(
        wait_for_file(cx, &inbox.join("photo.png")),
        "拖到文件夹上该拷进去"
    );
    assert!(
        dragged.exists(),
        "从系统拖来的只能拷：那些文件不属于我们，不能删"
    );
}

#[gpui_kit::test]
fn a_file_dropped_on_a_tab_lands_in_that_tabs_folder(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    std::fs::write(left.join("report.txt"), "hi").unwrap();

    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: None,
            focus_on_left: true,
            sync: false,
        },
    );

    let mut target = Point::default();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        target = center_of(window, ("tab", 1usize).into());
    });

    // 拖的是当前目录里的文件：落到第二个标签上就是"移到那个标签打开的目录"。
    drop_from_outside(cx, handle, target, &[left.join("report.txt")]);

    assert!(
        wait_for_file(cx, &right.join("report.txt")),
        "落到哪个标签就进哪个标签的目录"
    );
    assert!(
        left.join("report.txt").exists(),
        "模拟的是系统拖放，所以是拷贝：原处还在"
    );
}

/// 某一栏里指定名字的行在第几位（列表顺序会随排序变，按下标猜不稳）。
fn row_position(window: &mut gpui_kit::Window, name: &str) -> usize {
    row_names(window)
        .iter()
        .position(|row| row == name)
        .unwrap_or_else(|| panic!("列表里该有 {name}"))
}

#[gpui_kit::test]
fn hovering_a_folder_while_dragging_opens_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("inside.txt"), "x").unwrap();
    std::fs::write(dir.path().join("note.txt"), "x").unwrap();

    let (_session, handle) = open_in(cx, dir.path());

    let mut source = Point::default();
    let mut target = Point::default();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let note = row_position(window, "note.txt");
        let folder = row_position(window, "nested");
        source = center_of(window, ("row-drop", note).into());
        target = center_of(window, ("row-drop", folder).into());
    });

    // 拖到文件夹上面停住不松手。
    press_and_drag_to(cx, handle, source, target);

    // 才等了一小会儿：不该开，否则扫过一排文件夹会一路乱开。
    cx.background_executor
        .advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            row_names(window).iter().any(|row| row == "nested"),
            "刚扫过就进目录的话，一路划过去会跳得一塌糊涂"
        );
    });

    // 停够了：自己打开。
    cx.background_executor
        .advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            row_names(window),
            ["inside.txt"],
            "在同一个文件夹上停够时间，它该自己打开"
        );
    });
}

/// 窗格内部拖：按住 `from`，拖到 `to` 上方停住（还没松手）。
///
/// 移动要派两次：第一次才把"拖动"这个状态建立起来，
/// 第二次起鼠标才算是真的停在目标上。
fn press_and_drag_to(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    from: Point<gpui_kit::Pixels>,
    to: Point<gpui_kit::Pixels>,
) {
    step(cx, handle, |window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                position: from,
                button: gpui_kit::MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
    });
    for offset in [0., 1.] {
        step(cx, handle, |window, cx| {
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                    position: point(to.x + px(offset), to.y),
                    pressed_button: Some(gpui_kit::MouseButton::Left),
                    modifiers: Default::default(),
                }),
                cx,
            );
        });
    }
}

/// 窗格内部拖：按住在起始行，拖到目标行松手。
fn drag_between_rows(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    from: Point<gpui_kit::Pixels>,
    to: Point<gpui_kit::Pixels>,
) {
    press_and_drag_to(cx, handle, from, to);
    step(cx, handle, |window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                button: gpui_kit::MouseButton::Left,
                position: point(to.x + px(1.), to.y),
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    });
}

#[gpui_kit::test]
fn dragging_a_file_onto_a_folder_moves_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    std::fs::write(dir.path().join("note.txt"), "hi").unwrap();

    let (_session, handle) = open_in(cx, dir.path());

    let mut from = Point::default();
    let mut to = Point::default();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let note = row_position(window, "note.txt");
        let folder = row_position(window, "inbox");
        from = center_of(window, ("row-drop", note).into());
        to = center_of(window, ("row-drop", folder).into());
    });

    drag_between_rows(cx, handle, from, to);

    assert!(
        wait_for_file(cx, &inbox.join("note.txt")),
        "拖到文件夹上该进去"
    );
    assert!(
        wait_until_file_gone(cx, &dir.path().join("note.txt")),
        "自己拖自己是移动，原处不该还留一份"
    );
}

/// 拖一个已选中的项：拖起来的是**整批选中项**，不是那一个。
///
/// 这条同时钉住拖动载荷"一帧只算一次、行与行共享"的改法：
/// 载荷内容必须还是整批，不能被共享机制弄丢或弄串。
#[gpui_kit::test]
fn dragging_one_selected_file_carries_the_whole_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b").unwrap();

    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());
    let (handle, app) = open_app_of_width(cx, 1180.);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let a = row_position(window, "a.txt");
        window.click(("row-name", a), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // Shift 连选下一个：a、b 就成了整批。
        window.press("shift-down", cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt", "b.txt"],
        "先确认两个都选中了，再拖其中一个"
    );

    let mut from = Point::default();
    let mut to = Point::default();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let a = row_position(window, "a.txt");
        let folder = row_position(window, "inbox");
        from = center_of(window, ("row-drop", a).into());
        to = center_of(window, ("row-drop", folder).into());
    });

    drag_between_rows(cx, handle, from, to);

    assert!(wait_for_file(cx, &inbox.join("a.txt")), "拖的那一项该进去");
    assert!(
        wait_for_file(cx, &inbox.join("b.txt")),
        "同一批选中的 b.txt 也该跟着进去"
    );
}

/// 点在整批里的一员上、抬手没拖走：选中项塌回这一个。
///
/// 这是上一条的另一半：塌的动作从"按下"挪到了"真的只是一次点击"，
/// 单点收缩这件事本身不能跟着丢。
#[gpui_kit::test]
fn a_plain_click_collapses_a_multi_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());
    let (handle, app) = open_app_of_width(cx, 1180.);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("shift-down", cx);
    });
    assert_eq!(
        selected_names(cx, &app).len(),
        2,
        "先建立两选，再点其中一个"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt"],
        "点一下该塌回这一个"
    );
}

/// 造一个装了两个文本文件的 `bundle.zip`，放在 `dir` 里。
fn make_bundle(dir: &Path) -> std::path::PathBuf {
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.join(name), "hello").unwrap();
    }
    let zip = dir.join("bundle.zip");
    crate::archive::create(&zip, dir, &[dir.join("a.txt"), dir.join("b.txt")])
        .expect("造测试用的压缩包");
    zip
}

/// 面板里列出的条目名。
fn archive_entry_names(window: &mut gpui_kit::Window) -> Vec<String> {
    (0usize..64)
        .map(|index| window.try_find(("archive-entry", index)))
        .take_while(Option::is_some)
        .map(|entry| entry.unwrap().label().unwrap_or_default().to_string())
        .collect()
}

#[gpui_kit::test]
fn opening_a_zip_shows_what_is_inside(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    make_bundle(dir.path());
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let zip = row_position(window, "bundle.zip");
        window.click(("row-name", zip), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("archive").is_some(),
            "回车该打开压缩包面板，而不是交给系统"
        );
        assert_eq!(archive_entry_names(window), ["a.txt", "b.txt"]);
    });

    // Esc 关掉它（浮层优先于清空选中）。
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("archive").is_none(), "Esc 该关掉面板");
    });
}

#[gpui_kit::test]
fn extracting_a_zip_puts_the_files_in_a_folder_of_its_own(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    make_bundle(dir.path());
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let zip = row_position(window, "bundle.zip");
        window.click(("row-name", zip), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("archive-extract", cx);
    });

    // 解到"以包命名"的子目录里：直接摊在当前目录会把目录淹了。
    assert!(
        wait_for_file(cx, &dir.path().join("bundle").join("a.txt")),
        "该解到 bundle/ 里，而不是摊在当前目录"
    );
    assert!(dir.path().join("bundle.zip").exists(), "源压缩包不动");
}

#[gpui_kit::test]
fn packing_the_selection_makes_a_zip_next_to_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "1").unwrap();
    std::fs::write(dir.path().join("b.txt"), "2").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    // 压缩包搬进「更多」菜单了（工具栏只留四颗高频的），所以先开菜单。
    step(cx, handle, |window, cx| {
        window.click("op-more", cx);
        settle(window, cx);
    });
    // 菜单项按序号定位，而序号会随菜单内容漂移——先找出「打包成 Zip」
    // 在第几项，确认真的是它，再点。
    let archive = step_value(cx, handle, |window, _| {
        (0usize..16)
            .find(|&index| menu_item_label(window, index) == "打包成 Zip")
            .expect("「更多」菜单里该有「打包成 Zip」这一项")
    });
    step(cx, handle, |window, cx| {
        window.click(archive, cx);
    });

    let stem = dir
        .path()
        .file_name()
        .expect("临时目录有名字")
        .to_string_lossy()
        .into_owned();
    let zip = dir.path().join(format!("{stem}.zip"));
    assert!(wait_for_file(cx, &zip), "该在当前目录生成一个 zip");

    let listed = crate::archive::entries(&zip).expect("打出来的包读得出来");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["a.txt", "b.txt"]
    );
}

#[gpui_kit::test]
fn a_key_binding_can_be_changed_from_the_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (session, handle) = open_in(cx, dir.path());

    // 命令面板里搜"改键"能拉出快捷键那一组。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("改键：重命名", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("key:rename").is_some(),
            "该找得到「改键：重命名」这一项"
        );
        window.press("enter", cx);
    });

    // 浮层开出来，按一个新键。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("key-capture").is_some(),
            "该弹出录按键的浮层"
        );
        window.press("cmd-shift-k", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("key-capture").is_none(), "录完就该收起浮层");
    });

    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    assert_eq!(
        saved.keys.0.get("rename").map(String::as_str),
        Some("cmd-shift-k"),
        "新按键该记进配置：{:?}",
        saved.keys.0
    );
    assert!(
        !saved.keys.0.contains_key("trash"),
        "没改过的命令不该出现在配置里"
    );
}

#[gpui_kit::test]
fn backspace_in_the_capture_restores_the_factory_key(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("改键：重命名", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-shift-k", cx);
    });
    // 再改一次，这回用退格恢复出厂。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("改键：重命名", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("key-capture").is_some(),
            "第二次也该弹出浮层"
        );
        window.press("backspace", cx);
    });

    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    assert!(
        !saved.keys.0.contains_key("rename"),
        "退格该把这条清回出厂：{:?}",
        saved.keys.0
    );
}

// ---- 内嵌终端面板 ----

/// 终端面板里当前可见的文本（读的是格子区的 aria_label）。
fn terminal_text(window: &mut gpui_kit::Window) -> String {
    window
        .try_find("term-grid")
        .and_then(|grid| grid.label().map(|text| text.to_string()))
        .unwrap_or_default()
}

/// 开关键：绑定、放行、测试三处都得是同一个，所以从实体那边拿。
fn toggle_terminal(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    window.press(crate::term_entity::TOGGLE_KEY, cx);
}

#[gpui_kit::test]
fn the_terminal_panel_opens_and_the_same_key_closes_it(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("terminal").is_none(),
            "启动时不该自己开着终端面板"
        );
    });

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("terminal").is_some(),
            "开关键应该打开终端面板"
        );
    });

    // 面板一开就把焦点拿走了。焦点在终端里的时候，同一个键也必须还能收起它——
    // 否则用户就得先点回文件列表才能关掉，那一下很别扭。
    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("terminal").is_none(),
            "终端聚焦时开关键也要能收起面板"
        );
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_the_terminal_drawer(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("terminal").is_some(), "先打开终端");
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("terminal").is_none(), "再按一次该收起");
    });
    // 面板一开就把键盘拿走了，收起时它整个离开渲染树——接下来键盘在谁手上？
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "收起终端之后，快捷键该还灵");
    });
}

#[gpui_kit::test]
fn the_embedded_terminal_runs_a_real_command(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("terminal").is_some(), "面板要先打开");
    });
    // 真敲一条命令：敲进去的字节要真的顺着 PTY 到 shell 那儿。
    step(cx, handle, |window, cx| {
        window.input("echo wfiles-ok", cx);
        window.press("enter", cx);
    });

    // shell 是真正的子进程，不在测试调度器里，只能给它一点真实时间；
    // 每一轮都要画帧，输出是通过事件送回来的。
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline && !seen {
        cx.run_until_parked();
        seen = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                terminal_text(window).contains("wfiles-ok")
            })
            .expect("窗口还在");
        if !seen {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(seen, "终端里敲的命令应该真的跑起来，屏幕上要能看到输出");
}

#[gpui_kit::test]
fn the_terminal_panel_can_be_closed_from_its_own_button(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("term-close").is_some(),
            "面板上要有关闭按钮"
        );
        window.click("term-close", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("terminal").is_none(),
            "关闭按钮应该收起面板"
        );
    });
}

#[gpui_kit::test]
fn the_terminal_panel_is_reachable_from_the_command_palette(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("终端面板", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("app:toggle-terminal").is_some(),
            "命令面板里该能搜到终端面板"
        );
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("terminal").is_some(), "回车应该打开面板");
    });
}

#[gpui_kit::test]
fn the_terminal_grid_follows_the_panel_instead_of_keeping_the_default(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let rows = terminal_text(window).lines().count();
        // 面板高 220，扣掉标题栏与留白只剩十几行。出厂是 24 行——
        // 还是 24 就说明格子没量出来、行数没跟着面板改。
        assert!(
            rows > 4 && rows < 24,
            "行数该跟着面板高度走，实际是 {rows} 行"
        );
    });
}

#[gpui_kit::test]
fn dragging_across_the_terminal_selects_a_block(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        toggle_terminal(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let grid = window
            .try_find("term-grid")
            .expect("面板里该有格子区")
            .bounds();
        window.drag(
            gpui_kit::point(grid.origin.x + px(2.), grid.origin.y + px(2.)),
            gpui_kit::point(grid.origin.x + px(70.), grid.origin.y + px(30.)),
            cx,
        );
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("term-selection").is_some(),
            "拖过终端应该拖出一块选区"
        );
    });

    // 只点一下（不动）不算选中：那一下是"取消选中"。
    step(cx, handle, |window, cx| {
        window.click_at("term-grid", gpui_kit::point(px(2.), px(2.)), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("term-selection").is_none(),
            "原地点一下应该取消选区"
        );
    });
}

// ---- 搜索面板 ----

/// 搜索面板的开关键。与快捷键表、命令面板共用一处，所以从键位配置那边拿。
fn search_key(cx: &mut gpui_kit::App) -> String {
    crate::keymap::KeyMap::read(cx).key_for(crate::pane::PaneCommand::Search)
}

/// 结果区那句摘要：读屏软件与测试都靠它知道现在列出了什么。
fn search_results(window: &mut gpui_kit::Window) -> String {
    window
        .try_find("search-results")
        .and_then(|it| it.label().map(|text| text.to_string()))
        .unwrap_or_default()
}

/// 搜索面板状态行那句话（正在搜 / 找到几个 / 出错）。
fn search_status(window: &mut gpui_kit::Window) -> String {
    window
        .try_find("search-status")
        .and_then(|it| it.label().map(|text| text.to_string()))
        .unwrap_or_default()
}

/// 反复画帧，直到面板里某一处文字出现了要找的东西。
///
/// 两件事都得等：输入之后那 180 毫秒防抖是测试调度器里的虚拟时钟
/// （光 `run_until_parked` 不走时间，得自己往前推），
/// 而搜索本身跑在真正的 OS 线程上（见 [`crate::search::spawn`]），
/// 只能给它一点真实时间。
fn wait_for_text(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    read: fn(&mut gpui_kit::Window) -> String,
    needle: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        text = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                read(window)
            })
            .expect("窗口还在");
        if text.contains(needle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    text
}

fn wait_for_results(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    needle: &str,
) -> String {
    wait_for_text(cx, handle, search_results, needle)
}

fn wait_for_status(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    needle: &str,
) -> String {
    wait_for_text(cx, handle, search_status, needle)
}

/// 一个有几层文件和一点文本内容的目录。
fn searchable_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("建目录");
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("写文件");
    std::fs::write(dir.path().join("src/deep/inner.rs"), "fn inner() {}\n").expect("写文件");
    std::fs::write(dir.path().join("notes.txt"), "hello wfiles\nsecond line\n").expect("写文件");
    dir
}

#[gpui_kit::test]
fn the_search_panel_opens_and_escape_closes_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (_session, handle) = open_in(cx, dir.path());
    let key = cx.update(search_key);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("search").is_none(),
            "启动时不该自己开着搜索面板"
        );
    });

    step(cx, handle, move |window, cx| {
        window.press(&key, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("search").is_some(), "快捷键应该打开面板");
    });

    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("search").is_none(), "Esc 应该关掉面板");
    });
}

#[gpui_kit::test]
fn the_search_panel_finds_files_below_the_current_directory(cx: &mut TestAppContext) {
    let dir = searchable_dir();
    let (_session, handle) = open_in(cx, dir.path());
    let key = cx.update(search_key);

    step(cx, handle, move |window, cx| {
        window.press(&key, cx);
    });
    // 输入框是面板自己创建的，焦点已经在那儿了，直接敲。
    step(cx, handle, |window, cx| {
        window.input("*.rs", cx);
    });

    let listed = wait_for_results(cx, handle, "main.rs");
    assert!(
        listed.contains("main.rs") && listed.contains("inner.rs"),
        "两层以下的文件都该找到，实际：{listed}"
    );
}

#[gpui_kit::test]
fn a_content_search_looks_inside_files(cx: &mut TestAppContext) {
    let dir = searchable_dir();
    let (_session, handle) = open_in(cx, dir.path());
    let key = cx.update(search_key);

    step(cx, handle, move |window, cx| {
        window.press(&key, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("content:second", cx);
    });

    let listed = wait_for_results(cx, handle, "notes.txt");
    assert_eq!(listed, "notes.txt", "只有这个文件里有 second：{listed}");
}

#[gpui_kit::test]
fn a_query_typo_is_reported_instead_of_being_silently_ignored(cx: &mut TestAppContext) {
    let dir = searchable_dir();
    let (_session, handle) = open_in(cx, dir.path());
    let key = cx.update(search_key);

    step(cx, handle, move |window, cx| {
        window.press(&key, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("colour:red", cx);
    });

    let status = wait_for_status(cx, handle, "colour");
    assert!(status.contains("colour"), "认不出的字段该报出来：{status}");
}

#[gpui_kit::test]
fn choosing_a_hit_jumps_to_its_folder_and_selects_it(cx: &mut TestAppContext) {
    let dir = searchable_dir();
    let (_session, handle) = open_in(cx, dir.path());
    let key = cx.update(search_key);

    step(cx, handle, move |window, cx| {
        window.press(&key, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("inner", cx);
    });
    wait_for_results(cx, handle, "inner.rs");

    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("search").is_none(), "跳过去之后面板该收起");
        assert!(
            row_names(window).contains(&"inner.rs".to_string()),
            "该在它所在的目录里"
        );
        let name = window
            .try_find("detail-name")
            .and_then(|it| it.label().map(|text| text.to_string()))
            .unwrap_or_default();
        assert_eq!(name, "inner.rs", "跳过去的那个文件该是选中的：{name}");
    });
}

#[gpui_kit::test]
fn a_search_can_be_saved_run_again_and_forgotten(cx: &mut TestAppContext) {
    let dir = searchable_dir();
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.press("cmd-shift-f", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("*.rs", cx);
    });
    wait_for_results(cx, handle, "main.rs");

    // 存为… → 起个名字 → 回车。
    step(cx, handle, |window, cx| {
        window.click("search-save", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("search-naming").is_some(),
            "点存为该出现起名字那一行"
        );
        window.input("找源码", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("saved-search-0").is_some(),
            "存下来之后该有一枚小签"
        );
    });

    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    assert_eq!(saved.saved_searches.len(), 1);
    assert_eq!(saved.saved_searches[0].name, "找源码");
    assert_eq!(saved.saved_searches[0].query, "*.rs");

    // 把查询改坏，再点小签：查询该被填回来、结果该重新出现。
    step(cx, handle, |window, cx| {
        window.input("zzz", cx);
    });
    wait_for_status(cx, handle, "找到 0 个");
    step(cx, handle, |window, cx| {
        window.click("saved-search-0", cx);
    });
    let listed = wait_for_results(cx, handle, "main.rs");
    assert!(listed.contains("main.rs"), "点小签该把查询填回来：{listed}");

    // 存下来的搜索也能从命令面板里直接跑。
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("找源码", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("saved:找源码").is_some(),
            "命令面板里该能搜到存下来的搜索"
        );
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("search").is_some(), "回车该打开搜索面板");
    });

    // 最后把它忘掉。
    step(cx, handle, |window, cx| {
        window.click("forget-search-0", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("saved-search-0").is_none(),
            "忘掉之后小签该消失"
        );
    });
    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    assert!(saved.saved_searches.is_empty(), "设置里也该没了");
}

#[gpui_kit::test]
fn marking_a_file_sticks_and_the_mark_can_be_searched_for(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "hello\n").unwrap();
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-m", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("marks").is_some(),
            "Cmd+Alt+M 应该打开标记面板"
        );
        window.click("mark-color-red", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("工作 重要", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("marks").is_none(), "回车该收起面板");
    });

    // 落了盘才算真的记住：重启之后还得在。
    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    let mark = saved
        .marks
        .get(&dir.path().join("note.txt"))
        .expect("该有记号");
    assert_eq!(mark.color.as_deref(), Some("red"));
    assert_eq!(mark.tags, ["工作", "重要"]);

    // 标过的东西要能搜出来。
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-f", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("color:red", cx);
    });
    let listed = wait_for_results(cx, handle, "note.txt");
    assert!(
        listed.contains("note.txt"),
        "标成红色的该能被搜到：{listed}"
    );

    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });

    // 再开一次面板把标记清掉。
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-m", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("mark-clear", cx);
    });
    let saved = Settings::load_from(&session.path().join("settings.json")).expect("设置该落盘");
    assert!(
        saved.marks.get(&dir.path().join("note.txt")).is_none(),
        "清除之后不该留着记号：{:?}",
        saved.marks
    );
}

#[gpui_kit::test]
fn the_mark_panel_is_reachable_from_the_command_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "x").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("标记", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("pane:Mark").is_some(),
            "命令面板里该能搜到标记"
        );
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("marks").is_some(), "回车应该打开面板");
    });
}

#[gpui_kit::test]
fn the_search_panel_is_reachable_from_the_command_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("搜索", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("pane:Search").is_some(),
            "命令面板里该能搜到搜索"
        );
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("search").is_some(), "回车应该打开面板");
    });
}

// ---- 分析：文件夹大小 / 重复文件 / 校验和 ----

/// 分析面板是在真线程上算的，结果靠事件送回来：给它一点真实时间，
/// 每一轮都得画帧——不画帧事件就不会被消费。
fn wait_for_analyze_row(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    index: usize,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window
                    .try_find(("analyze-row", index))
                    .map(|row| row.label().unwrap_or_default().to_string())
            })
            .expect("窗口还在");
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui_kit::test]
fn the_analyze_panel_adds_up_a_folder(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "12345").unwrap();
    std::fs::write(dir.path().join("b.txt"), "123").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-alt-i", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("analyze").is_some(), "该打开分析面板");
    });

    // 头一行是那个目录自己：8 B（5 + 3），两个文件。
    let head = wait_for_analyze_row(cx, handle, 0).expect("该算出这一行");
    assert!(head.contains("8 B"), "大小该是两个文件之和：{head}");
    assert!(head.contains("2 个文件"), "该报文件个数：{head}");

    // 底下跟着最大的几个文件：a.txt 比 b.txt 大，排在前面。
    let biggest = wait_for_analyze_row(cx, handle, 1).expect("该列出最大的文件");
    assert!(biggest.contains("a.txt"), "最大的该是 a.txt：{biggest}");
}

#[gpui_kit::test]
fn the_analyze_panel_hashes_the_selected_file(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("abc.txt"), "abc").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let row = row_position(window, "abc.txt");
        window.click(("row-name", row), cx);
    });
    // 选中了文件，面板该直接落在"校验和"这一页。
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-i", cx);
    });

    let label = wait_for_analyze_row(cx, handle, 0).expect("该算出校验和");
    assert!(
        label.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "SHA-256 该是 abc 的那一个：{label}"
    );
}

#[gpui_kit::test]
fn the_analyze_panel_finds_duplicate_files(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("one")).unwrap();
    std::fs::create_dir_all(dir.path().join("two")).unwrap();
    std::fs::write(dir.path().join("one/a.txt"), "一样的内容").unwrap();
    std::fs::write(dir.path().join("two/b.txt"), "一样的内容").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-alt-i", cx);
    });
    // 切到"重复文件"那一页。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("analyze-tab", 1usize), cx);
    });

    let label = wait_for_analyze_row(cx, handle, 0).expect("该找到一组重复");
    assert!(label.contains("2 个副本"), "两个一样的文件是一组：{label}");
}

#[gpui_kit::test]
fn escape_closes_the_analyze_panel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "12345").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-alt-i", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("analyze").is_some(), "该打开面板");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("analyze").is_none(), "Esc 该关掉面板");
    });
}

// ---- 目录比较与同步 ----

/// 造一对待比较的目录，放在 `root` 下面（单栏时选中这两个就能比）。
fn compare_pair(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let left = root.join("l");
    let right = root.join("r");
    std::fs::create_dir_all(&left).unwrap();
    std::fs::create_dir_all(&right).unwrap();
    std::fs::write(left.join("same.txt"), "一样的内容").unwrap();
    std::fs::write(right.join("same.txt"), "一样的内容").unwrap();
    std::fs::write(left.join("only-left.txt"), "只有左").unwrap();
    (left, right)
}

fn wait_for_compare_row(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    index: usize,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window
                    .try_find(("compare-row", index))
                    .map(|row| row.label().unwrap_or_default().to_string())
            })
            .expect("窗口还在");
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui_kit::test]
fn comparing_two_folders_lists_what_differs(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    compare_pair(root.path());
    let (_session, handle) = open_in(cx, root.path());

    // 选中 l 与 r 两个目录：单栏时"比较"比的就是它们。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_some(), "该打开比较面板");
    });

    let label = wait_for_compare_row(cx, handle, 0).expect("该列出差异");
    assert!(label.contains("only-left.txt"), "该列出左边独有的：{label}");
    assert!(label.contains("只有左"), "该写明只有哪一边有：{label}");
}

#[gpui_kit::test]
fn syncing_copies_the_difference_into_the_other_folder(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let (_left, right) = compare_pair(root.path());
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-b", cx);
    });
    let label = wait_for_compare_row(cx, handle, 0);
    assert!(label.is_some(), "先得比出差异");

    // 第一个同步按钮是"→ 拷到右"。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("compare-sync", 0usize), cx);
    });

    assert!(
        wait_for_file(cx, &right.join("only-left.txt")),
        "该把左边独有的拷到右边"
    );
}

#[gpui_kit::test]
fn escape_closes_the_compare_panel(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    compare_pair(root.path());
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_some(), "该打开面板");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_none(), "Esc 该关掉面板");
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_the_compare_panel(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    compare_pair(root.path());
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_some(), "先打开比较面板");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_none(), "Esc 该关掉");
    });
    // 面板开着的时候焦点在它身上；关掉之后键盘得有人接手。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "关掉比较面板之后，快捷键该还灵");
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_toggling_the_compare_panel_off(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    compare_pair(root.path());
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_some(), "先打开比较面板");
        window.press("cmd-alt-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("compare").is_none(), "同一个键再按该收起");
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "同一个键收起面板之后，快捷键该还灵");
    });
}

// ---- 文件内容比较：按行 / 细到字符 / 十六进制 ----

/// 面板在两个文件读完之后才出结果，那个读取在真线程上：
/// 给它一点真实时间，每一轮都画帧——不画帧事件就不会被消费。
fn wait_for_diff_row(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    index: usize,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window
                    .try_find(("diff-row", index))
                    .map(|row| row.label().unwrap_or_default().to_string())
            })
            .expect("窗口还在");
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui_kit::test]
fn comparing_two_files_shows_which_line_changed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "第一行\n第二行\n第三行\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "第一行\n改过的第二行\n第三行\n").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    // 单栏选中两个文件，"比较文件内容"比的就是它们。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-shift-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("diff").is_some(), "该打开内容比较面板");
    });

    // 三行都在（上下文留着才好对照），中间那行标"改"。
    let changed = wait_for_diff_row(cx, handle, 1).expect("该比出差异行");
    assert!(changed.starts_with("改"), "中间那行该标成改过：{changed}");
    assert!(
        changed.contains("改过的第二行"),
        "该显示右边的新内容：{changed}"
    );

    let status = step_value(cx, handle, |window, _| {
        window
            .try_find("diff-status")
            .and_then(|node| node.label().map(|label| label.to_string()))
    });
    let status = status.expect("该有状态行");
    assert!(status.contains("改了 1 行"), "该报改了一行：{status}");
}

#[gpui_kit::test]
fn the_diff_panel_switches_to_bytes_for_binary_files(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // 开头带 0 字节 → 判成二进制，只能按字节看。
    std::fs::write(dir.path().join("a.bin"), [0u8, 1, 2, 3, 4]).unwrap();
    std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 9, 4]).unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-shift-b", cx);
    });

    let _ = wait_for_diff_row(cx, handle, 0).expect("该列出字节行");
    let status = step_value(cx, handle, |window, _| {
        window
            .try_find("diff-status")
            .and_then(|node| node.label().map(|label| label.to_string()))
    });
    let status = status.expect("该有状态行");
    assert!(
        status.contains("个字节不一样") && status.contains("偏移 3"),
        "二进制该按字节报差异：{status}"
    );
}

#[gpui_kit::test]
fn escape_closes_the_diff_panel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "一样\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "一样\n").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-shift-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("diff").is_some(), "该打开面板");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("diff").is_none(), "Esc 该关掉面板");
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_the_diff_panel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "一样\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "一样\n").unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-shift-b", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("diff").is_some(), "先打开内容比较面板");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("diff").is_none(), "Esc 该关掉");
    });
    // 面板开着的时候焦点在它身上；关掉之后键盘得有人接手。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "关掉内容比较面板之后，快捷键该还灵");
    });
}

// ---- 可选元数据列 ----

/// 元数据是后台探出来的：给它一点真实时间，每一轮都画帧。
///
/// 这一格一开列就存在（里面可以是空的，列宽才不会跳），
/// 所以等到"有字"才算数。
fn wait_for_meta_row(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    index: usize,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        cx.run_until_parked();
        let found = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                window
                    .try_find(("row-meta", index))
                    .map(|row| row.label().unwrap_or_default().to_string())
            })
            .expect("窗口还在");
        if found.as_ref().is_some_and(|label| !label.is_empty()) || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 从命令面板里挑一种元数据列。走的是真界面路径：
/// 面板、匹配、点击，全过了才说明这条功能是接上的。
fn pick_meta_column(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    query: &'static str,
    id: &'static str,
) {
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input(query, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(id, cx);
    });
}

#[gpui_kit::test]
fn the_metadata_column_shows_image_dimensions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pic.png");
    image::RgbImage::from_fn(320, 180, |x, y| image::Rgb([x as u8, y as u8, 128]))
        .save(&path)
        .unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    pick_meta_column(cx, handle, "图片尺寸", "app:meta-image");

    let label = wait_for_meta_row(cx, handle, 0).expect("该探出图片尺寸");
    assert!(label.contains("320 × 180"), "该报像素尺寸：{label}");
}

#[gpui_kit::test]
fn the_metadata_column_shows_git_status(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .output()
        .is_ok_and(|output| output.status.success());
    if !initialized {
        // 没装 git 的机器上这条没意义，别把测试跑红。
        return;
    }
    std::fs::write(dir.path().join("new.txt"), "hello").unwrap();
    let (_session, handle) = open_in(cx, dir.path());
    let position = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_position(window, "new.txt")
    });

    pick_meta_column(cx, handle, "Git 状态", "app:meta-git");

    let label = wait_for_meta_row(cx, handle, position).expect("该读出 Git 状态");
    assert!(label.contains("未跟踪"), "新文件该是未跟踪：{label}");
}

#[gpui_kit::test]
fn the_metadata_column_can_be_turned_off_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pic.png");
    image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([1, 2, 3]))
        .save(&path)
        .unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    pick_meta_column(cx, handle, "图片尺寸", "app:meta-image");
    assert!(
        wait_for_meta_row(cx, handle, 0).is_some(),
        "开了那一列就该有那一格"
    );

    pick_meta_column(cx, handle, "不显示元数据列", "app:meta-off");
    let gone = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find(("row-meta", 0usize)).is_none()
    });
    assert!(gone, "关掉之后这一格该消失");
}

// ---------------------------------------------------------------------------
// P4 远程连接
// ---------------------------------------------------------------------------

/// 从命令面板打开连接对话框：面板里挑那条命令点一下。
fn open_connect_dialog(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>) {
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("连接远程服务器", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("app:connect", cx);
    });
}

/// 状态栏上那一句（远程位置、正在取…、几个项目都写在那里）。
fn status_text(window: &mut gpui_kit::Window) -> String {
    window
        .try_find("pane-status")
        .and_then(|it| it.label().map(|label| label.to_string()))
        .unwrap_or_default()
}

#[gpui_kit::test]
fn the_connect_dialog_refuses_an_empty_address(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    open_connect_dialog(cx, handle);

    // 空地址按"连接"：对话框留着，并且说清楚缺什么，
    // 而不是弹一个"连接失败"让人去猜。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("connect-ok").is_some(), "该弹出连接对话框");
        window.click("connect-ok", cx);
    });
    let error = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .try_find("connect-error")
            .and_then(|it| it.label().map(|label| label.to_string()))
            .unwrap_or_default()
    });
    assert_eq!(error, "服务器地址不能为空", "该说出缺什么：{error}");
}

#[gpui_kit::test]
fn switching_to_s3_reveals_the_region_field(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    open_connect_dialog(cx, handle);

    // 默认按 WebDAV 填：地区是 S3 才有的概念，摆在那里只会让人填错。
    let before = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("connect-region").is_some()
    });
    assert!(!before, "WebDAV 时不该问地区");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("connect-kind-s3", cx);
    });

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("connect-region").is_some()
    });
    assert!(after, "切到 S3 之后该能填地区：它是签名的一部分");
}

#[gpui_kit::test]
fn switching_to_sftp_reveals_the_key_field(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    open_connect_dialog(cx, handle);

    // 只有 SFTP 能用私钥登录，别的协议摆这一栏是纯噪音。
    let before = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("connect-key").is_some()
    });
    assert!(!before, "WebDAV 时不该问私钥");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("connect-kind-sftp", cx);
    });

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("connect-key").is_some()
    });
    assert!(after, "切到 SFTP 之后该能填私钥文件");
}

#[gpui_kit::test]
fn pasting_into_a_remote_folder_uploads_over_http(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let root = tempfile::tempdir().expect("临时目录");
    let local = root.path().join("a.txt");
    std::fs::write(&local, "hello").unwrap();

    // 真的起一个服务器、真的发一次 PUT：
    // 远程粘贴最容易错的正是"地址拼成什么样"，只有真发一次才试得出来。
    let server = crate::remote::fake::start();
    let ops = cx.update(|cx| {
        gpui_kit::init(cx);
        Settings::update(cx, |settings| {
            settings
                .remotes
                .push(crate::remote::fake::config(&server.url));
        });
        let remotes = cx.new(|_| crate::remotes::Remotes::new());
        cx.new(|_| crate::ops_entity::Ops::new(Some(remotes)))
    });
    cx.background_executor.allow_parking();

    cx.update(|cx| {
        ops.update(cx, |ops, cx| {
            ops.cut(vec![local.clone()], cx);
            ops.request_paste(crate::remote::location("t1", "folder"), cx);
        });
    });
    wait_until_idle(cx, &ops);
    let message = message_of(cx, &ops);
    assert_eq!(message, "移动完成 1 项", "实际是 {message}");
    assert!(!local.exists(), "剪切之后本地那份该没了");

    let seen = server.seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|it| it.starts_with("PUT /dav/folder/a.txt")),
        "该真的上传了一次：{seen:?}"
    );
}

#[gpui_kit::test]
fn a_remote_target_that_already_exists_asks_before_writing(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let root = tempfile::tempdir().expect("临时目录");
    // 名字故意和假服务器上已有的那个一样：这一粘必然撞名。
    let local = root.path().join("notes.txt");
    std::fs::write(&local, "hello").unwrap();

    let server = crate::remote::fake::start();
    let ops = cx.update(|cx| {
        gpui_kit::init(cx);
        Settings::update(cx, |settings| {
            settings
                .remotes
                .push(crate::remote::fake::config(&server.url));
        });
        let remotes = cx.new(|_| crate::remotes::Remotes::new());
        cx.new(|_| crate::ops_entity::Ops::new(Some(remotes)))
    });
    cx.background_executor.allow_parking();

    cx.update(|cx| {
        ops.update(cx, |ops, cx| {
            ops.copy(vec![local.clone()], cx);
            ops.request_paste(crate::remote::location("t1", "folder"), cx);
        });
    });
    wait_until_idle(cx, &ops);

    // 判重是问服务器的，所以结论要等那一趟回来：回来之后该弹冲突对话框。
    let conflict = cx.update(|cx| ops.read(cx).conflict().map(|it| it.collisions));
    assert_eq!(conflict, Some(1), "撞名了就该先问：{conflict:?}");
    // 没问清楚之前不该动服务器上的东西。
    let seen = server.seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|it| it.starts_with("PROPFIND")),
        "只该问，不该写：{seen:?}"
    );
}

#[gpui_kit::test]
fn a_saved_connection_navigates_to_its_remote_location(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| {
            settings.remotes.push(crate::remote::Config::new(
                "jg".to_string(),
                "坚果云".to_string(),
                crate::remote::Kind::WebDav,
                // 端口 9 是 discard 端口：连不上，且是本机立刻拒绝，
                // 不会真的把请求发到网络上去。
                "http://127.0.0.1:9/dav/".to_string(),
                "me".to_string(),
            ));
        });
    });

    let label = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .try_find(("remote", 0usize))
            .and_then(|row| row.label().map(|label| label.to_string()))
            .unwrap_or_default()
    });
    assert_eq!(label, "坚果云", "侧栏该列出存下来的连接：{label}");

    step(cx, handle, |window, cx| {
        window.click(("remote", 0usize), cx);
    });

    // 切过去之后状态栏必须写明这是远程位置（或正在取），
    // 列表长得跟本地一样，不说的话用户会以为自己在本地目录里。
    let status = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        status_text(window)
    });
    assert!(
        status.contains("正在取这个目录") || status.contains("远程位置"),
        "该写明这是远程位置：{status}"
    );
}

#[gpui_kit::test]
fn a_cloud_sync_folder_is_called_out_in_the_sidebar_and_the_details(cx: &mut TestAppContext) {
    use crate::cloud::{Provider, SyncRoot};

    let _session = isolate_session();
    let root = tempfile::tempdir().expect("临时目录");
    let sync = root.path().join("Dropbox");
    std::fs::create_dir_all(&sync).unwrap();
    std::fs::write(sync.join("notes.txt"), "hello").unwrap();
    // 真机器上装没装云盘客户端由不得测试决定，所以喂一份假的进去。
    crate::cloud::install(vec![SyncRoot {
        provider: Provider::Dropbox,
        path: sync.clone(),
    }]);

    let handle = open_app(cx);

    let label = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .try_find(("cloud", 0usize))
            .and_then(|row| row.label().map(|label| label.to_string()))
            .unwrap_or_default()
    });
    assert_eq!(label, "Dropbox", "侧栏该列出本机的云盘：{label}");

    step(cx, handle, |window, cx| {
        // 云盘那一段在折叠线以下：先滚到它，再点。
        scroll_sidebar(window, cx);
        window.click(("cloud", 0usize), cx);
    });

    // 进了同步目录：状态栏得写明，这里删改会牵动另一台机器上的同一份。
    let status = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        status_text(window)
    });
    assert!(status.contains("Dropbox"), "该写明在云盘里：{status}");

    // 选中那个文件：详情里写明是哪家云盘、什么状态。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    let cloud = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        // id 里的 `cloud` 是**稳定英文标识**，不是标签：标签跟着界面语言变
        // （中文叫"云盘"、英文叫"Cloud"），而 id 不能变——否则切到英文之后
        // 这个元素就找不到了，而那行内容一个字都没变。
        window
            .try_find("detail-cloud")
            .and_then(|row| row.label().map(|label| label.to_string()))
            .unwrap_or_default()
    });
    assert_eq!(cloud, "Dropbox · 已同步到本地", "实际是 {cloud}");
}

#[gpui_kit::test]
fn the_webdav_preset_fills_in_a_domestic_provider(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let (handle, app) = open_app_of_width(cx, 1180.);
    open_connect_dialog(cx, handle);

    // WebDAV 才摆"常用服务"：SFTP/S3/FTP 各有各的服务商，没有这样一句能填的通用地址。
    let shown = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("connect-presets").is_some()
    });
    assert!(shown, "WebDAV 时该有常用服务");

    // 读输入框里真正的值而不是界面上的字：没填时界面显示的是占位提示，
    // 而占位提示恰好也是一个地址，光看界面分不清"填了"和"没填"。
    let before = cx.update(|cx| app.read(cx).connect_url(cx));
    assert!(before.is_empty(), "刚打开时该是空的：{before}");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("connect-preset-jianguoyun", cx);
    });

    // 点一下就把地址填上了，不用去记那个 URL。
    let after = cx.update(|cx| app.read(cx).connect_url(cx));
    assert_eq!(after, "https://dav.jianguoyun.com/dav/", "实际是 {after}");
}

#[gpui_kit::test]
fn a_speed_limit_slows_the_transfer_down(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let from = root.path().join("from.bin");
    // 1 MB：限速 500 KB/s 时，头一秒的免费额度用完之后还得再等约一秒。
    std::fs::write(&from, vec![7u8; 1_000_000]).unwrap();

    let ops = new_ops(cx);

    // 先不限速跑一趟做对照：同样的数据该一眨眼就完。
    let free = run_one_copy(cx, &ops, &from, &root.path().join("free"));
    assert!(
        free < Duration::from_millis(800),
        "不限速时不该慢：{free:?}"
    );

    // 走设置这一条路：它才是限速的唯一来源（面板改的也是它）。
    cx.update(|cx| Settings::update(cx, |settings| settings.speed_limit = 500_000));
    let limited = run_one_copy(cx, &ops, &from, &root.path().join("slow"));
    assert!(
        limited > Duration::from_millis(800),
        "限速 500 KB/s 传 1 MB 该慢下来：{limited:?}"
    );

    // 撤掉限速之后立刻按不限速跑：不用等重启。
    cx.update(|cx| Settings::update(cx, |settings| settings.speed_limit = 0));
    let again = run_one_copy(cx, &ops, &from, &root.path().join("fast"));
    assert!(
        again < Duration::from_millis(800),
        "撤销限速该立刻生效：{again:?}"
    );
}

/// 拷同一个文件到一个新目录，返回花了多久。
fn run_one_copy(
    cx: &mut TestAppContext,
    ops: &gpui_kit::Entity<Ops>,
    from: &Path,
    dest: &Path,
) -> Duration {
    std::fs::create_dir_all(dest).unwrap();
    cx.update(|cx| {
        ops.update(cx, |ops, cx| {
            ops.copy(vec![from.to_path_buf()], cx);
            ops.request_paste(dest.to_path_buf(), cx);
        })
    });
    let started = Instant::now();
    wait_until_idle(cx, ops);
    started.elapsed()
}

#[gpui_kit::test]
fn the_sidebar_lists_volumes_and_one_click_goes_there(cx: &mut TestAppContext) {
    // 和 volumes 模块里的单测共用一把锁：两边改的是同一份 override。
    let _serial = crate::volumes::test_guard();
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("USB");
    std::fs::create_dir(&disk).unwrap();
    std::fs::write(disk.join("a.txt"), "插上的盘").unwrap();

    // 跑测试的机器上大概率一个卷都没有，真扫出来是空的；
    // 钉一个假的才能验那几行。
    crate::volumes::set_override(vec![crate::volumes::Volume {
        name: "USB".to_string(),
        path: disk.clone(),
        // 探不出容量就不显示那一行（光盘、网络共享就是这样）。
        capacity: None,
    }]);
    let (session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        let row = window
            .try_find(("volume", 0usize))
            .expect("侧栏该列出刚挂上的那个卷");
        // `label()` 取的是读屏文案：光念 "USB" 不知道那是块盘。
        assert_eq!(row.label(), Some("磁盘：USB"), "读屏要听得懂这是块盘");
    });

    // 点一下就该过去——挂载点跟普通目录没有任何区别，
    // 这正是"不做专用传输 UI"要保证的那件事。
    // 点的是行本身。外面那层 `volume-drop` 只是拖放用的壳，
    // 它没调 `.test_support()`，观察表里没有它的条目（只有别的元素
    // 路过时把它记成祖先）——`place-drop` 同样点不到，这里没必要学它。
    step(cx, handle, |window, cx| {
        window.click(("volume", 0usize), cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 单栏布局里没有 `pane-slot` 那层壳（多包一层会把虚拟列表挤扁），
        // 所以直接按 `row-name` 找，不走 `row_names_in`。
        let name = window
            .try_find(("row-name", 0usize))
            .and_then(|row| row.label().map(|label| label.to_string()))
            .unwrap_or_default();
        assert_eq!(name, "a.txt", "该切到 U 盘里并看到内容，实际是 {name:?}");
    });

    drop(session);
    crate::volumes::clear_override();
}

/// 卷上"还剩多少"：探得到才显示，探不到就只显示名字。
///
/// 判据盯**那一行字本身**（由 `Capacity::describe` 拼）。只断言"侧栏有容量区"
/// 是挑错层——名字本来就在那儿，多一层也能满足。
#[gpui_kit::test]
fn a_volume_shows_how_much_is_left_only_when_it_knows(cx: &mut TestAppContext) {
    let _serial = crate::volumes::test_guard();
    const GB: u64 = 1024 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Backup")).unwrap();
    std::fs::create_dir(dir.path().join("CD")).unwrap();

    crate::volumes::set_override(vec![
        crate::volumes::Volume {
            name: "备份盘".to_string(),
            path: dir.path().join("Backup"),
            capacity: Some(crate::volumes::Capacity {
                total: 500 * GB,
                free: 120 * GB,
            }),
        },
        crate::volumes::Volume {
            name: "光盘".to_string(),
            path: dir.path().join("CD"),
            // 光盘 / 网络共享 / 空读卡器都可能报不出容量。
            capacity: None,
        },
    ]);

    let (session, handle) = open_in(cx, dir.path());
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let text = window
            .try_find(("volume-capacity", 0usize))
            .and_then(|node| node.label().map(|label| label.to_string()))
            .unwrap_or_default();
        assert_eq!(
            text, "可用 120.0 GB / 共 500.0 GB",
            "探得到容量就该显示出来，实际是 {text:?}"
        );
        // 探不出来的那块：不显示那一行，**更不许编一个 0 出来**。
        assert!(
            window.try_find(("volume-capacity", 1usize)).is_none(),
            "探不出容量的卷不该显示那一行"
        );
    });

    drop(session);
    crate::volumes::clear_override();
}

/// 窗口矮到侧栏一定装不下时，底部那颗「本地文件系统」仍得点得到。
///
/// 真事：磁盘那节每行多了一行「剩多少」，侧栏就装不下了，底部那颗被顶到
/// 窗口外面。旧结构下它照样"可见"（没有裁剪祖先），于是判据绿着、界面残着；
/// 现在的结构把它钉在底部、中间那几段自己滚。
///
/// 用**矮窗口**而不是塞十几块假盘来制造溢出：`volumes::set_override` 是
/// 进程级的，塞一串假盘会把并行跑的其他侧栏判据一起顶到折叠线以下。
#[gpui_kit::test]
fn a_short_window_keeps_the_sidebar_footer_reachable(cx: &mut TestAppContext) {
    let root = crate::volumes::machine_root();
    // 360 px：连「位置」那几条都装不完，云盘与远程更是远在折叠线以下。
    let (handle, app) = open_app_of_size(cx, 1180., 360.);
    step(cx, handle, settle);

    let before = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        !same_dir(&before[0], &root),
        "起点不该已经就是本机根（{}），否则这条测试什么都没验到",
        root.display(),
    );

    // `click` 先查可见性：被顶出可见区的那颗在这里就报 not visible。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("sidebar-local", cx);
    });
    step(cx, handle, settle);

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        same_dir(&live[0], &root),
        "窗口再矮，底部那颗也得在：实际停在 {}",
        live[0].display(),
    );
}

#[gpui_kit::test]
fn an_empty_machine_shows_no_volumes_section(cx: &mut TestAppContext) {
    let _serial = crate::volumes::test_guard();
    crate::volumes::set_override(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find(("volume", 0usize)).is_none(),
            "一个卷都没挂时不该留一个空分组"
        );
    });

    crate::volumes::clear_override();
}

/// 崩溃恢复的核心承诺：重启后看到的不是"那个目录"，
/// 而是"那个目录里我正处理着的那批文件"。
///
/// 一次测试里连开两次应用：第二趟只有隔离与恢复文件还在，
/// 会话照样指向同一个目录——这跟崩溃后重开是等价的，
/// 而且省掉"关掉窗口再开"那种 GPUI 测试里做不到的收尾。
#[gpui_kit::test]
fn a_selection_survives_a_restart(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    // 全程只隔离一次：隔离里含一把进程级锁，第二次调用会死锁。
    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());

    // 第一趟：选中 a 和 b——这就是"用户当时在做的事"。
    let (handle, app) = open_app_of_width(cx, 1180.);
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // Shift 连选第二行：这是崩溃恢复最该扛住的用法——
        // 用户正准备拷一批，不是一两个。
        window.press("shift-down", cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt", "b.txt"],
        "先确认选中确实建立了"
    );

    // 第二趟：重新启动。什么都不用带——恢复文件自己就在那儿。
    let (_handle, app) = open_app_of_width(cx, 1180.);
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt", "b.txt"],
        "重开之后当时选中的那批文件该还在——不然用户得在列表里再点一遍"
    );
}

/// 目录里的东西变了：被删掉的那个不该被"恢复"成选中。
///
/// `Cmd+C` 去拷一个不存在的路径是明确的错误操作，比"少恢复一个"坏得多。
#[gpui_kit::test]
fn a_file_deleted_before_the_restart_is_not_selected_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());

    let (handle, app) = open_app_of_width(cx, 1180.);
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("shift-down", cx);
    });
    assert_eq!(selected_names(cx, &app), vec!["a.txt", "b.txt"]);

    // 崩了、还没重开，这期间 b.txt 被删了。
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();

    let (_handle, app) = open_app_of_width(cx, 1180.);
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt"],
        "没了的文件不该被恢复成选中"
    );
}

/// 取消了选择之后重启：不该凭空恢复出一批选中项。
#[gpui_kit::test]
fn clearing_the_selection_means_nothing_to_restore(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b").unwrap();
    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());

    let (handle, app) = open_app_of_width(cx, 1180.);
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    // Esc 清掉选择：用户明说了"不要了"。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("escape", cx);
    });
    assert!(selected_names(cx, &app).is_empty(), "Esc 该把选择清空");

    let (_handle, app) = open_app_of_width(cx, 1180.);
    assert!(
        selected_names(cx, &app).is_empty(),
        "用户自己清掉的就别恢复回来"
    );
}

/// 关掉的标签，它的恢复现场就该立刻从盘上消失。
///
/// 不等到下次启动才清：那份文件是每次选择变化都写的，用户开着应用
/// 连关十个标签，那十个目录的现场就会一直躺着——而只要碰巧再打开
/// 同一个目录，就会冒出一批用户早就忘了的选中项。
#[gpui_kit::test]
fn closing_a_tab_forgets_what_would_have_been_restored(cx: &mut TestAppContext) {
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    for dir in [one.path(), two.path()] {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }
    let isolation = isolate_session();
    // 两个标签各自在自己的目录里。
    crate::session::Session {
        tabs: vec![one.path().to_path_buf(), two.path().to_path_buf()],
        active: 1,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&isolation.path().join("session.json"))
    .expect("写测试会话");

    let (handle, app) = open_app_of_width(cx, 1180.);
    // 两个目录都要有现场，否则"没牵连"那条断言本身就无从谈起——
    // 一个从没被选中过的目录，本来就没东西可清。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 切标签是 ctrl-tab（cmd-1/2 是列表↔网格，别搞混）。
        window.press("ctrl-tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("ctrl-tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt"],
        "第二栏该选中它那一份"
    );
    // 比对要用窗格实际认到的路径，不是 `tempdir` 给的那串：
    // macOS 上 `/var` 是指向 `/private/var` 的符号链接，两串指向同一个目录，
    // 但字符串不等——按原样比对会看成"功能坏了"。
    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    let set = crate::restore::RestoreSet::load();
    assert!(
        live.iter().all(|dir| set.at(dir).is_some()),
        "两栏都该有现场，实际是{live:?}，现场在 {:?}",
        set.items.iter().map(|it| &it.dir).collect::<Vec<_>>()
    );

    // 关掉当前这一个标签（活动的是 1，也就是 two）。
    let before = cx.update(|cx| app.read(cx).tab_paths(cx));
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-w", cx);
    });
    step(cx, handle, settle);

    // 关掉的是哪个，靠前后差集算出来，而不是猜"活动的一定是 two"：
    // 猜错的话断言会变成"另一个目录的现场没清掉"，
    // 报错信息指向的方向完全是反的。
    let after = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(after.len(), before.len() - 1, "该正好关掉一个标签");
    let set = crate::restore::RestoreSet::load();
    for gone in before
        .iter()
        .filter(|dir| !after.iter().any(|kept| same_dir(kept, dir)))
    {
        assert!(
            set.at(gone).is_none(),
            "标签都关了（{gone:?}），现场还留在盘上，下次打开这个目录会冒出一批旧选中项"
        );
    }
    for kept in &after {
        assert!(
            set.at(kept).is_some(),
            "还开着的那个目录不该被牵连（{kept:?}）"
        );
    }
}

/// 命令行说开到哪儿，窗口就落在哪儿——不用用户再点一遍。
///
/// 这是"能写脚本调起来"里最要紧的一条：`wfiles /某个目录` 开出来的窗口
/// 必须真的在那个目录上，而不是"恢复上次会话，然后你猜"。
#[gpui_kit::test]
fn a_command_line_target_replaces_the_restored_tab(cx: &mut TestAppContext) {
    let asked = tempfile::tempdir().unwrap();
    let restored = tempfile::tempdir().unwrap();
    for dir in [asked.path(), restored.path()] {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }
    // 先摆一份**别的**目录的会话：命令行的目标要压过它，
    // 不然这条测试验的还是会话恢复。
    let isolation = isolate_session();
    write_session(isolation.path(), restored.path());

    let (_handle, app) = open_app_at(
        cx,
        1180.,
        Some(crate::cli::Launch {
            target: crate::cli::Target::Open(asked.path().to_path_buf()),
            new_tab: false,
        }),
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        paths.iter().any(|dir| same_dir(dir, asked.path())),
        "那一栏该开在命令行给的目录上，实际是 {paths:?}"
    );
    assert!(
        !paths.iter().any(|dir| same_dir(dir, restored.path())),
        "命令行说了去哪儿，上次会话那个目录就不该还占着一栏：{paths:?}"
    );
}

/// 命令行给的是一个文件（或者 `--reveal`）：开到父目录，选中它。
///
/// 同一段里先开一次**不带目标**的做对照：不然"选中了目标"可能只是
/// 恢复现场恰好也是它，这条测试就什么都没钉住。
#[gpui_kit::test]
fn a_command_line_file_is_selected_in_its_folder(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "目标.txt"] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    let isolation = isolate_session();
    write_session(isolation.path(), dir.path());

    // 对照：没有目标就没有选中项。
    let (_handle, app) = open_app_at(cx, 1180., None);
    assert!(
        selected_names(cx, &app).is_empty(),
        "没有命令行目标，就不该凭空选中什么"
    );

    let target = dir.path().join("目标.txt");
    let (_handle, app) = open_app_at(
        cx,
        1180.,
        Some(crate::cli::Launch {
            target: crate::cli::Target::Reveal(target.clone()),
            new_tab: false,
        }),
    );
    assert_eq!(
        selected_names(cx, &app),
        vec!["目标.txt"],
        "定位到的就该是选中的那一个，而且只有它"
    );
}

/// `--new-tab`：多开一栏，不是把原来那一栏挪走——焦点落在新的这一栏上。
#[gpui_kit::test]
fn a_new_tab_target_joins_the_rest_instead_of_replacing_it(cx: &mut TestAppContext) {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("a.txt"), "a").unwrap();
    std::fs::write(second.path().join("目标.txt"), "hi").unwrap();
    let isolation = isolate_session();
    write_session(isolation.path(), first.path());

    let (_handle, app) = open_app_at(
        cx,
        1180.,
        Some(crate::cli::Launch {
            target: crate::cli::Target::Reveal(second.path().join("目标.txt")),
            new_tab: true,
        }),
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(
        paths.len(),
        2,
        "原来那一栏还在，目标是另加的一栏：{paths:?}"
    );
    assert!(
        paths.iter().any(|dir| same_dir(dir, first.path())),
        "会话里那一栏不该被顶掉：{paths:?}"
    );
    // 读的是**活动**那一栏的选中项，所以这也验了焦点落在新开的那一栏。
    assert_eq!(
        selected_names(cx, &app),
        vec!["目标.txt"],
        "新开的那一栏该是焦点所在"
    );
}

// ---- 键盘焦点 ----
//
// 这一组钉的是同一件事：**焦点所属的元素一旦不在渲染树里，应用级快捷键就
// 全哑**。gpui 找不着那个节点，就把按键派发到窗口根节点，而挂在应用根元素
// 上的 `on_action` 一个都不在那条路径上——切栏、关栏、开标签、命令面板、
// 分栏全按不出来。症状不报错、不留痕（"键盘突然不灵了"，点一下界面又好了），
// 所以判据只能是"再按一个快捷键，看它生不生效"，不是去读界面上写了什么。

/// 标签栏上有几栏。应用级快捷键还灵不灵，看它。
fn tab_count(window: &mut gpui_kit::Window) -> usize {
    (0usize..16)
        .filter(|index| window.try_find(("tab", *index)).is_some())
        .count()
}

/// 开一扇窗，会话里摆上几个临时目录，活动栏是第 `active` 个。
fn open_tabs(
    cx: &mut TestAppContext,
    active: usize,
    names: &[&str],
) -> (
    tempfile::TempDir,
    SessionIsolation,
    gpui_kit::WindowHandle<Root>,
) {
    let root = tempfile::tempdir().expect("临时目录");
    let mut tabs = Vec::new();
    for name in names {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).expect("造目录");
        tabs.push(dir);
    }
    let (isolation, handle) = open_session(
        cx,
        &Session {
            tabs,
            active,
            split: None,
            focus_on_left: true,
            sync: false,
        },
    );
    (root, isolation, handle)
}

#[gpui_kit::test]
fn the_shortcuts_survive_switching_tabs(cx: &mut TestAppContext) {
    let (_root, _session, handle) = open_tabs(cx, 0, &["甲", "乙"]);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("ctrl-tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            tab_count(window),
            3,
            "切了标签之后 Cmd+T 该还灵——换掉的那一栏已经不在渲染树里，\
             键盘得交给新那一栏"
        );
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_a_tab(cx: &mut TestAppContext) {
    // 连着关两栏：第一次关掉的那一栏连着它的焦点句柄一起没了，
    // 第二次还按得出来，才说明键盘当时有主。
    let (_root, _session, handle) = open_tabs(cx, 1, &["甲", "乙", "丙"]);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 3);
        window.press("cmd-w", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2);
        window.press("cmd-w", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            tab_count(window),
            1,
            "关掉一栏之后 Cmd+W 该还灵——关掉的那一栏已经在渲染树外了"
        );
    });
}

#[gpui_kit::test]
fn the_shortcuts_work_as_soon_as_the_window_opens(cx: &mut TestAppContext) {
    // 会话里活动栏不是最后那一栏：开窗时每一栏建出来都会自己抓一次焦点，
    // 抓错地方的话，用户刚打开就按不出快捷键。
    let (_root, _session, handle) = open_tabs(cx, 0, &["甲", "乙", "丙"]);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 4, "刚开窗就该按得出 Cmd+T");
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_clicking_a_tab_in_the_tab_bar(cx: &mut TestAppContext) {
    let (_root, _session, handle) = open_tabs(cx, 1, &["甲", "乙"]);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("tab", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 3, "点标签栏换了栏之后，快捷键该还灵");
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_the_command_palette(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            tab_count(window),
            2,
            "关掉命令面板之后 Cmd+T 该还灵——面板的输入框没了，键盘得有人接手"
        );
    });
}

#[gpui_kit::test]
fn the_shortcuts_survive_closing_a_dialog(cx: &mut TestAppContext) {
    // 甲：批量重命名（Cmd+Shift+R）→ Esc 关掉。
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-r", cx);
        settle(window, cx);
    });
    // 对话框是下一帧才挂上的（按下去只是个"要开"的事件），所以断言分两步：
    // 不然这一步量到的可能还是空，下面的 Esc 就变成"关了个寂寞"。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "Cmd+Shift+R 该开出批量重命名"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_none(),
            "Esc 该关掉批量重命名"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-t", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "关掉批量重命名之后 Cmd+T 该还灵");
    });

    // 乙：重命名（F2）→ Esc 关掉。
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("down", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("f2", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "F2 该开出重命名对话框"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_none(),
            "Esc 该关掉重命名对话框"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-t", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "关掉重命名对话框之后 Cmd+T 该还灵");
    });
}

#[gpui_kit::test]
fn switching_tabs_leaves_the_keyboard_in_the_dialog(cx: &mut TestAppContext) {
    // 反过来的那一条：对话框自己拿着键盘的时候换栏，键盘**不该**被抢走——
    // 抢走了用户就没法继续在里面打字，连 Esc 都送不到对话框手上。
    let dir = overlay_fixture();
    let elsewhere = tempfile::tempdir().expect("临时目录");
    std::fs::write(elsewhere.path().join("别处.txt"), "x").unwrap();
    let (isolation, handle) = open_session(
        cx,
        &Session {
            tabs: vec![dir.path().to_path_buf(), elsewhere.path().to_path_buf()],
            active: 0,
            split: None,
            focus_on_left: true,
            sync: false,
        },
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2);
        window.press("down", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("f2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "按 F2 该开出重命名对话框"
        );
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("ctrl-tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 快捷键确实执行了（换栏那件事发生了）……
        let saved = Session::load_from(&isolation.path().join("session.json")).expect("会话该落盘");
        assert_eq!(saved.active, 1, "Ctrl+Tab 该换到另一栏");
        // ……但键盘还在对话框手里。
        assert!(
            window.try_find("overlay-card").is_some(),
            "换栏不该顺手把对话框弄没"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_none(),
            "Esc 该还是对话框的——键盘被抢到窗格上的话，它根本收不到这个键"
        );
    });
}

#[gpui_kit::test]
fn a_new_tab_does_not_steal_the_keyboard_from_a_modal(cx: &mut TestAppContext) {
    // 模态开着时按 Cmd+T：新栏建出来会自己抓一次焦点（`Pane::new`），把键盘
    // 从模态手里抢走——面板/对话框从此"看着开着、其实按不动"：打不进字，
    // 连 Esc 都送不进去，只能点一下别的界面把它弄回来。
    //
    // 甲：命令面板。
    let root = tempfile::tempdir().expect("临时目录");
    let (_session, handle) = open_in(cx, root.path());
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_some(),
            "Cmd+P 该打开命令面板"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "Cmd+T 该照常开出一栏");
        assert!(
            window.try_find("palette-overlay").is_some(),
            "开栏不该顺手把面板关掉"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "Esc 该还是面板的——键盘被新栏抢走的话，它根本收不到这个键"
        );
    });

    // 乙：重命名对话框（模态不止面板一个，这条钉的是"别只修面板"）。
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("down", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("f2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "F2 该开出重命名对话框"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-t", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(tab_count(window), 2, "Cmd+T 该照常开出一栏");
        assert!(
            window.try_find("overlay-card").is_some(),
            "开栏不该顺手把对话框关掉"
        );
    });
    step(cx, handle, |window, cx| {
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_none(),
            "Esc 该还是对话框的——键盘被新栏抢走的话，它根本收不到这个键"
        );
    });
}

/// 设置对话框：Cmd+, 开、Esc 关，关完键盘要回到窗格。
///
/// 卡片里没有输入框，"键盘进没进来"从界面上看不出来——判据只能是两个键：
/// 开着时 Esc 能收掉它（说明按键冒到了浮层根节点），关掉后 ↓ 能选中行
/// （说明键盘回到了窗格）。后一条钉的是 AGENTS 记着的那个坑：卡片是**当场
/// 从渲染树里撤掉**的，焦点要是还挂在它身上，窗格级快捷键会整片变哑，
/// 不报错、不留痕。
#[gpui_kit::test]
fn the_settings_dialog_opens_with_cmd_comma_and_esc_gives_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    std::fs::write(dir.path().join("b.txt"), "x").expect("造文件");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-,", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "Cmd+, 该开出设置卡"
        );
        assert_eq!(a11y_label_at(window, "overlay-title"), "设置");
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_none(),
            "Esc 该收掉设置卡"
        );
        window.press("down", cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt".to_string()],
        "关掉设置之后，窗格级的按键该还灵（↓ 得能选中第一行）"
    );
}

/// 设置卡里点一下就能改设置。判据钉在**设置本身**上，不看配色：
/// "哪一颗亮着"是画出来的，改了没有只有数据说了算。
#[gpui_kit::test]
fn clicking_a_choice_in_the_settings_dialog_changes_the_setting(cx: &mut TestAppContext) {
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    // 先摆一个"以前选过"的状态：终端与语言默认就是"自动"，不先改一下的话，
    // 点那两颗跟没点一样，这条测试就什么都没验到。
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| {
            settings.terminal = "wezterm".to_string();
            settings.set_locale(crate::i18n::Locale::En);
            settings.speed_limit = 0;
        });
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-,", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("setting-terminal-auto", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            Settings::peek(cx, |settings| settings.terminal.clone()),
            "auto",
            "点了「自动」之后设置里该是 auto"
        );
        assert!(
            window.try_find("overlay-card").is_some(),
            "改一项不该顺手收掉卡片——设置是「改完接着看下一项」的地方"
        );
        window.click("setting-speed-1000000", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            Settings::peek(cx, |settings| settings.speed_limit),
            1_000_000,
            "点了「1 MB/s」之后限速该跟着变"
        );
        window.click("setting-locale-auto", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            Settings::peek(cx, |settings| settings.locale.clone()),
            "",
            "点「跟随系统」该存空串，不是把当前语言写回去——写回去等于从此不再跟随"
        );
        assert!(window.try_find("overlay-card").is_some());
    });
}

/// 存着的终端本机没有时（换了机器、装过又卸了），那颗仍然要看得见。
///
/// 不补这一颗的话，四颗里一颗都不亮——用户看见的是"我的设置丢了"，
/// 而不是"这个终端没了"，前者会让人到处找。
#[gpui_kit::test]
fn a_terminal_that_is_no_longer_installed_still_shows_up(cx: &mut TestAppContext) {
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| {
            settings.terminal = "no-such-term".to_string()
        });
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-,", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            a11y_label_at(window, "setting-terminal-no-such-term"),
            "no-such-term（当前）",
            "存着但本机没有的终端该看得见，并且标着当前"
        );
    });
    // 换得掉：点「自动」。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("setting-terminal-auto", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            Settings::peek(cx, |settings| settings.terminal.clone()),
            "auto",
            "点得掉那个已经不存在的终端"
        );
    });
}

/// 设置也能从命令面板进：不是所有人都记得住 Cmd+,。
#[gpui_kit::test]
fn the_settings_dialog_opens_from_the_palette(cx: &mut TestAppContext) {
    let dir = overlay_fixture();
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("设置", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.click("app:settings", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("overlay-card").is_some(),
            "面板里点「设置」该开出设置卡"
        );
    });
}

/// 副窗**只读不写**：会话、窗口布局、别的目录的恢复现场都动不得。
///
/// 这三样是"两扇窗"今天就会踩的三个文件：两扇各写一份 `session.json`
/// （重启后恢复成哪一份全看谁最后导航过）、`layout` 互相挪窗口、
/// 副窗启动时那一遍清扫把主窗正在看的目录的现场整个删掉。
#[gpui_kit::test]
fn a_secondary_window_leaves_the_session_and_the_layout_alone(cx: &mut TestAppContext) {
    let running = tempfile::tempdir().unwrap(); // 主窗在看的地方
    let elsewhere = tempfile::tempdir().unwrap(); // 副窗要去的地方
    for dir in [running.path(), elsewhere.path()] {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }
    let isolation = isolate_session();
    write_session(isolation.path(), running.path());

    // 主窗此刻的现场：记着一处窗口位置，主窗看的那个目录里有个选中项。
    let layout = crate::settings::Layout {
        x: 40.,
        y: 30.,
        width: 900.,
        height: 700.,
    };
    Settings {
        layout: Some(layout),
        ..Settings::default()
    }
    .save_to(&isolation.path().join("settings.json"))
    .expect("写设置");
    let mut set = crate::restore::RestoreSet::default();
    set.note(
        running.path().to_path_buf(),
        Vec::new(),
        Some(running.path().join("a.txt")),
    );
    set.save_to(&isolation.path().join("restore.json"))
        .expect("写恢复现场");

    let session_path = isolation.path().join("session.json");
    let settings_path = isolation.path().join("settings.json");
    let session_before = std::fs::read(&session_path).expect("读会话");

    // 副窗：没抢到位子的那一扇。`--new-tab`：别人的会话照旧恢复，目标另加
    // 一栏——跟主窗长得一样正是它该有的样子，不同的只是**写作的权限**。
    let (handle, app) = open_app_starting(
        cx,
        1180.,
        Starting {
            launch: Some(crate::cli::Launch {
                target: crate::cli::Target::Open(elsewhere.path().to_path_buf()),
                new_tab: true,
            }),
            fresh: false,
            primary: false,
        },
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(paths.len(), 2, "会话那一栏 + 命令行那一栏：{paths:?}");
    assert!(
        paths.iter().any(|dir| same_dir(dir, elsewhere.path())),
        "副窗该开在命令行给的地方：{paths:?}"
    );
    // 这里**不**断言"清扫没删别人的现场"：副窗恢复的是同一份会话，
    // 主窗看的那一栏本来就在它自己的标签里，清扫扫不到什么。
    // 那条断言在 `a_new_window_does_not_copy_the_last_session` 里——
    // 只有"另起一扇、不恢复会话"的副窗才会把别的目录当垃圾清掉。

    // 动它一下：开一栏。在主窗里这一步会写会话（`add_tab` → `sync_session`）。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-t", cx);
    });
    step(cx, handle, settle);
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(paths.len(), 3, "这一步得真的办成了，不然下面钉不住东西");

    // 再关掉**主窗正在看**的那一栏（焦点先挪过去，再点一行落下焦点——
    // 跟用户一样：先选中一个文件，再关这一栏）。
    // 在主窗里这一步会清掉那个目录的现场（`forget_restore`）。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("ctrl-tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-w", cx);
    });
    step(cx, handle, settle);
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(paths.len(), 2, "该正好关掉一栏：{paths:?}");
    assert!(
        !paths.iter().any(|dir| same_dir(dir, running.path())),
        "关掉的该是 `running` 那一栏，不然下面那条断言打的不是这一枪：{paths:?}"
    );

    assert_eq!(
        std::fs::read(&session_path).expect("读会话"),
        session_before,
        "副窗不该动会话文件"
    );
    let saved = Settings::load_from(&settings_path).expect("设置还在");
    assert_eq!(
        saved.layout,
        Some(layout),
        "副窗不该记窗口位置——记下去就是替主窗挪窗口"
    );
    assert!(
        crate::restore::RestoreSet::load()
            .at(running.path())
            .is_some(),
        "副窗关掉那一栏时，把主窗正在看的那个目录的现场删了"
    );
}

/// `--new-window` 要的是一扇**干净**的窗：给哪儿就开哪儿，不给就开主页，
/// 上次那排标签不跟着复制一份过来。
#[gpui_kit::test]
fn a_new_window_does_not_copy_the_last_session(cx: &mut TestAppContext) {
    let last_time = tempfile::tempdir().unwrap();
    let also_last_time = tempfile::tempdir().unwrap();
    let asked = tempfile::tempdir().unwrap();
    for dir in [last_time.path(), also_last_time.path(), asked.path()] {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }
    let isolation = isolate_session();
    // 上次开着两栏：真去恢复会话的话，这两栏会原样跟着出现。
    crate::session::Session {
        tabs: vec![
            last_time.path().to_path_buf(),
            also_last_time.path().to_path_buf(),
        ],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&isolation.path().join("session.json"))
    .expect("写测试会话");
    // 上次那两个目录各留一份现场：新窗不恢复会话，它眼里只有命令行给的那一个，
    // 一启动就替别人清场的话，这两条会被当成垃圾删掉。
    let mut set = crate::restore::RestoreSet::default();
    for dir in [last_time.path(), also_last_time.path()] {
        set.note(dir.to_path_buf(), Vec::new(), Some(dir.join("a.txt")));
    }
    set.save_to(&isolation.path().join("restore.json"))
        .expect("写恢复现场");

    // 给了路径：新窗里只有它这一栏。
    let (_handle, app) = open_app_starting(
        cx,
        1180.,
        Starting {
            launch: Some(crate::cli::Launch {
                target: crate::cli::Target::Open(asked.path().to_path_buf()),
                new_tab: false,
            }),
            fresh: true,
            primary: false,
        },
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(paths.len(), 1, "新窗里只有命令行说的那一栏：{paths:?}");
    assert!(same_dir(&paths[0], asked.path()));
    let set = crate::restore::RestoreSet::load();
    assert!(
        set.at(last_time.path()).is_some(),
        "新窗只开着 `asked`，却把别人（主窗）正在看的目录的现场扫了"
    );

    // 没给路径：从主页起。
    let (_handle, app) = open_app_starting(
        cx,
        1180.,
        Starting {
            launch: None,
            fresh: true,
            primary: false,
        },
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(paths.len(), 1, "一栏主页，不是上次那两栏：{paths:?}");
    let home = dirs::home_dir().expect("系统得有个主页目录");
    assert!(
        same_dir(&paths[0], &home),
        "该落在主页，实际是 {:?}",
        paths[0]
    );
}

/// 第二次启动把那句话交给已经开着的窗口——真的走一趟回环 socket。
///
/// 这条是单实例转发的全链路：主实例抢位子、挂门牌、听；"第二份"读门牌、
/// 连上去、说一句话、等一声"收到了"；界面那一头把请求落下去。
/// 中间任何一段（解析、队列、跨线程唤醒）坏了，这里都会挂。
#[gpui_kit::test]
fn a_second_launch_lands_in_the_window_that_is_already_open(cx: &mut TestAppContext) {
    let running = tempfile::tempdir().unwrap();
    let asked = tempfile::tempdir().unwrap();
    std::fs::write(running.path().join("a.txt"), "a").unwrap();
    std::fs::write(asked.path().join("目标.txt"), "hi").unwrap();
    let isolation = isolate_session();
    write_session(isolation.path(), running.path());

    let (handle, app) = open_app_of_width(cx, 1180.);
    // 这一份就是主实例：位子抢到手，门牌挂出去。
    let claim = crate::instance::claim().expect("隔离目录里该抢得到位子");
    step(cx, handle, |window, cx| {
        app.update(cx, |app, cx| app.serve_forwarding(claim, window, cx));
    });

    // 第二次启动：说的话交给那一份。这里不等界面——回话是在"排进队列"
    // 之后才发的，所以下面等的才是界面那一头。
    let asked_path = asked.path().join("目标.txt");
    let handed = crate::instance::hand_off(&second_launch(crate::cli::Launch {
        target: crate::cli::Target::Reveal(asked_path.clone()),
        new_tab: false,
    }));
    assert!(handed, "已经开着的那一份该应答");

    assert!(
        wait_until(cx, |cx| {
            cx.update(|cx| app.read(cx).tab_paths(cx))
                .iter()
                .any(|dir| same_dir(dir, asked.path()))
        }),
        "转过来的目标该落在当前那一栏上"
    );
    assert_eq!(
        selected_names(cx, &app),
        vec!["目标.txt"],
        "说要定位的那一个也该选中"
    );
}

/// 转发过来的 `--new-tab`：另开一栏，不是把当前那一栏挪走。
#[gpui_kit::test]
fn a_forwarded_new_tab_joins_the_rest(cx: &mut TestAppContext) {
    let running = tempfile::tempdir().unwrap();
    let asked = tempfile::tempdir().unwrap();
    std::fs::write(asked.path().join("b.txt"), "b").unwrap();
    let isolation = isolate_session();
    write_session(isolation.path(), running.path());

    let (handle, app) = open_app_of_width(cx, 1180.);
    let claim = crate::instance::claim().expect("隔离目录里该抢得到位子");
    step(cx, handle, |window, cx| {
        app.update(cx, |app, cx| app.serve_forwarding(claim, window, cx));
    });

    let handed = crate::instance::hand_off(&second_launch(crate::cli::Launch {
        target: crate::cli::Target::Open(asked.path().to_path_buf()),
        new_tab: true,
    }));
    assert!(handed, "已经开着的那一份该应答");

    assert!(
        wait_until(cx, |cx| {
            cx.update(|cx| app.read(cx).tab_paths(cx)).len() == 2
        }),
        "该多出一栏"
    );
    let paths = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        paths.iter().any(|dir| same_dir(dir, running.path())),
        "原来那一栏不该被顶掉：{paths:?}"
    );
    assert!(
        paths.get(1).is_some_and(|dir| same_dir(dir, asked.path())),
        "新开的那一栏该开在目标上，而且排在最后（焦点所在）：{paths:?}"
    );
}

/// 等一件事在界面上成立。
///
/// 转发是在后台线程上排的队，界面这一头要走一趟跨线程唤醒——测试里
/// 只能给它一点真实时间（同 `wait_for`，理由也一样）。
fn wait_until(cx: &mut TestAppContext, mut ready: impl FnMut(&mut TestAppContext) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        cx.run_until_parked();
        if ready(cx) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 两个路径是不是同一个目录（可以是不同的路径字符串）。
fn same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// "第二次启动"手里那份命令行：只带一个目标，别的什么都没给。
fn second_launch(launch: crate::cli::Launch) -> crate::cli::Cli {
    crate::cli::Cli {
        launch: Some(launch),
        ..Default::default()
    }
}

/// 写一份"打开这个目录"的会话。
fn write_session(state_dir: &Path, dir: &Path) {
    crate::session::Session {
        tabs: vec![dir.to_path_buf()],
        active: 0,
        split: None,
        focus_on_left: true,
        sync: false,
    }
    .save_to(&state_dir.join("session.json"))
    .expect("写测试会话");
}

/// 当前那一栏选中了哪些文件的文件名（按列表顺序）。
///
/// 从实体读而不是从界面读：选中标记画在 `ListItem` 上，而那个元素不可观察，
/// 界面上根本没有"这一行被选中了"这个事实可查。
fn selected_names(cx: &mut TestAppContext, app: &gpui_kit::Entity<WFilesApp>) -> Vec<String> {
    cx.update(|cx| app.read(cx).selected_in_active_pane(cx))
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_string())
        })
        .collect()
}

/// 顶部两行（地址栏 + 工具栏）在任何窗口宽度下都不越界。
///
/// 这条守着**结构**：Win11 Explorer 是两行，地址栏一行、工具栏一行。
/// 原来 WFiles 把两者挤在一行，那一排固有宽度 908 px，比任何一档窗口都
/// 宽——于是我先砍按钮、再加侧栏折叠、还给筛选框设上限，全是给这个错误
/// 结构打的补丁。
///
/// 拆开之后各行的固定部分（实测按钮宽度）：
/// - 地址栏行：3 颗导航 96 + 刷新 32 + 筛选框 200 = 328 px
/// - 工具栏行：4 颗操作 96 + 更多 24 + 3 颗视图 72 + 详情 24 = 216 px
///
/// 所以两行在 520 px 窗口（侧栏收起后 472 px）里都装得下，**一行都不需要
/// 裁**。判据是每行的右边缘不超过窗口右边界。
#[gpui_kit::test]
fn both_top_rows_stay_inside_the_window_at_every_width(cx: &mut TestAppContext) {
    for width in [1180.0f32, 900.0, 700.0, 520.0] {
        let dir = tempfile::tempdir().expect("临时目录");
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
        }
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
        step(cx, handle, settle);

        for row in ["address-bar", "command-bar"] {
            let bounds = step_value(cx, handle, |window, _| {
                window
                    .try_find(row.to_string())
                    .map(|element| element.bounds())
            })
            .unwrap_or_else(|| panic!("窗口 {width}：找不到 {row}（element id 变了？）"));

            assert!(
                bounds.origin.x + bounds.size.width <= px(width),
                "窗口 {width}：{row} 右边缘在 x={} px（宽 {}），超出窗口——\
                 地址栏与工具栏分两行时每一行都装得下，越界说明又挤回一行了",
                bounds.origin.x + bounds.size.width,
                bounds.size.width,
            );
        }
    }
}

/// 地址栏与工具栏必须是**两行**，而且地址栏在上。
///
/// 这条守的是结构本身而不是某一处的位置：把两者合并回一行是"修不动宽度"
/// 时最容易想到的招，而它恰恰是问题的起因。用户在这两行里找的是两种
/// 不同的东西——"我在哪"和"我能做什么"。
#[gpui_kit::test]
fn the_address_bar_and_the_command_bar_are_separate_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    for i in 0..30 {
        std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
    }
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);

    let (address, command) = step_value(cx, handle, |window, _| {
        let read = |id: &str| window.try_find(id.to_string()).map(|e| e.bounds());
        (read("address-bar"), read("command-bar"))
    });
    let address = address.expect("地址栏行");
    let command = command.expect("工具栏行");

    assert_eq!(
        address.origin.x, command.origin.x,
        "两行左边缘应当对齐（都是内边距起头）",
    );
    assert_eq!(
        address.size.width, command.size.width,
        "两行应当一样宽——它们是同一个容器里的两个兄弟",
    );
    assert!(
        address.origin.y < command.origin.y,
        "地址栏必须在工具栏**上面**：地址栏 y={}，工具栏 y={}",
        address.origin.y,
        command.origin.y,
    );
}

/// 筛选框在地址栏行的最右端，工具栏那一行不碰它。
///
/// 筛选框是"看我眼前这些"，跟"我在哪"是两回事。挤在导航按钮旁边会让人
/// 以为它也是导航的一部分，于是打完字不知道该往哪看。
#[gpui_kit::test]
fn the_filter_box_sits_at_the_right_end_of_the_address_bar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    for i in 0..30 {
        std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
    }
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 1180.);
    step(cx, handle, settle);

    let (address, filter, last_op) = step_value(cx, handle, |window, _| {
        let read = |id: &str| window.try_find(id.to_string()).map(|e| e.bounds());
        (read("address-bar"), read("filter-slot"), read("op-paste"))
    });
    let address = address.expect("地址栏行");
    let filter = filter.expect("筛选框");
    let last_op = last_op.expect("粘贴按钮（工具栏行最后一颗常显）");

    // 两行各自独立：工具栏那颗粘贴按钮的行位置与筛选框差一整行。
    assert!(
        (last_op.origin.y - filter.origin.y).abs() > px(20.),
        "粘贴按钮 y={} 与筛选框 y={} 挨得太近——它们看起来还在同一行",
        last_op.origin.y,
        filter.origin.y,
    );
    // 筛选框贴着地址栏右端。
    let right_gap = address.origin.x + address.size.width - (filter.origin.x + filter.size.width);
    assert!(
        right_gap <= px(16.),
        "筛选框离地址栏右边缘还有 {right_gap:?} px，它该贴着右端",
    );
    // 宽度上限还在（200 px 设计值），窄窗口下才让位。
    assert!(
        filter.size.width <= px(200.),
        "筛选框宽 {} px，超出设计的 200 px 上限",
        filter.size.width,
    );
}

/// 侧栏折叠仍然管用——两条边框加起来 470 px，窄窗口下要还给内容区。
///
/// 这一条跟两行布局是配套的：拆成两行之后每一行都装得下，但
/// 「侧栏 210 + 详情板 260 = 470 px 的固定边框」还在，520 px 的窗口里
/// 内容区只剩 50 px。折叠是那道保险，不是拆两行的替代品。
#[gpui_kit::test]
fn collapsing_the_fixed_borders_gives_the_content_room(cx: &mut TestAppContext) {
    for width in [900.0f32, 700.0, 520.0] {
        let dir = tempfile::tempdir().expect("临时目录");
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
        }
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);

        // 两侧都收：侧栏 Cmd+B，详情板 Cmd+Alt+O。
        //
        // **必须分两步**：同一个 `step` 里连按两个键，第一个触发的
        // `cx.notify()` 会重画，第二个键就落在新的焦点上了——实测
        // 详情板那一下被吞掉，界面上还亮着。
        step(cx, handle, |window, cx| {
            window.press("cmd-b", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("cmd-alt-o", cx);
            settle(window, cx);
        });

        let address = step_value(cx, handle, |window, _| {
            window.try_find("address-bar").map(|e| e.bounds())
        })
        .expect("地址栏行");
        assert_eq!(
            address.origin.x,
            px(48.),
            "窗口 {width}：侧栏收起后应当是 48 px 的图标条，地址栏从那儿开始，\
             实测 x={}",
            address.origin.x,
        );
        assert_eq!(
            address.size.width,
            px(width - 48.),
            "窗口 {width}：两侧收起后地址栏应当正好占满剩下的宽度",
        );
    }
}

/// 收起侧栏之后位置入口一个都不能少，而且能点回去。
///
/// 只留一颗展开按钮的话，用户收起侧栏就等于把侧栏废掉了——找不到个人
/// 文件夹、桌面、下载。
#[gpui_kit::test]
fn a_collapsed_sidebar_keeps_its_places(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");

    let (_isolation, handle) = open_in(cx, dir.path());
    step(cx, handle, |window, cx| {
        window.press("cmd-b", cx);
        settle(window, cx);
    });

    // 收起之后「位置」那几条还在。带序号查——收起态里每一条的
    // element id 是 `("collapsed-place", 序号)`，用裸字符串查不到。
    let collapsed_places = step_value(cx, handle, |window, _| {
        (0usize..4)
            .filter(|&index| {
                window
                    .try_find(("collapsed-place", index))
                    .map(|e| e.bounds().size.width > px(0.))
                    .unwrap_or(false)
            })
            .count()
    });
    assert!(
        collapsed_places > 0,
        "侧栏收起后「位置」那几条不见了——收起了就等于废掉了",
    );

    // 点那颗展开按钮能回去，展开按钮自己也在。
    step(cx, handle, |window, cx| {
        window.click("sidebar-collapse", cx);
        settle(window, cx);
    });
    assert!(
        step_value(cx, handle, |window, _| {
            window.try_find("sidebar-expand").map(|e| e.bounds())
        })
        .is_some(),
        "收起态里那颗展开按钮不见了，用户再也回不去",
    );
}

/// 详情板收起来要**整块不画**，不是画成 0 宽。
///
/// 判据是元素在观察表里彻底不见了：留着 0 宽的分隔线和不可见的边框，
/// 看起来像"界面坏了"。
#[gpui_kit::test]
fn collapsing_the_details_panel_removes_it_entirely(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");

    let (_isolation, handle) = open_in(cx, dir.path());
    step(cx, handle, settle);

    assert!(
        step_value(cx, handle, |window, _| {
            window.try_find("details-panel").map(|e| e.bounds())
        })
        .is_some(),
        "详情板默认就该在（这是这条测试的前提）",
    );

    step(cx, handle, |window, cx| {
        window.click("toggle-details", cx);
        settle(window, cx);
    });
    let hidden = step_value(cx, handle, |window, _| {
        window.try_find("details-panel").map(|e| e.bounds())
    });
    assert!(
        hidden.is_none(),
        "点了收起之后详情板还在（宽 {} px）——要么没收，要么收成了 0 宽的壳",
        hidden.map(|b| b.size.width).unwrap_or(px(0.)),
    );
}

/// 详情板与窗格**不许重叠**，双栏时也一样。
///
/// 这是用户报上来的那条：双栏 + 详情板开着时，「详细信息」的标题被筛选框
/// 压住。根因是面板原来挂在**窗格里面**——每栏 484 px 被切掉 260 px 之后
/// 主列只剩 224 px，而地址栏那一行自己就要 292 px、列表表头要 512 px，
/// 多出来的部分画到了面板上（GPUI 默认不裁子元素，面板又是透明的，
/// 底下的字照样看得见）。
///
/// 现在的结构是"侧栏 | 窗格 | 详情板"三块并排：面板是窗格的**兄弟**。
/// 判据两条：面板左边缘不早于最后一栏的右边缘（1180 / 900 / 520 三种宽度
/// 都量）；每一栏自己的子元素都收在自己栏里——这条只在 900 px 以上量，
/// 520 px 的双栏每栏只剩 24.5 px，装不下是结构事实。
#[gpui_kit::test]
fn the_details_panel_never_overlaps_a_pane(cx: &mut TestAppContext) {
    for width in [1180.0f32, 900.0, 520.0] {
        let dir = tempfile::tempdir().expect("临时目录");
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
        }
        let (_session, handle) = open_session_at_width(
            cx,
            &Session {
                tabs: vec![dir.path().to_path_buf(), dir.path().to_path_buf()],
                active: 0,
                split: Some(1),
                focus_on_left: true,
                sync: false,
            },
            width,
        );
        step(cx, handle, settle);

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            let panel = window.try_find("details-panel").expect("详情板").bounds();
            assert!(
                panel.size.width >= px(200.),
                "窗口 {width}：详情板只剩 {} px 宽——它被挤扁了",
                panel.size.width,
            );
            for slot in 0usize..2 {
                let limit = pane_right(window, slot);
                assert!(
                    panel.origin.x >= limit - px(1.),
                    "窗口 {width}：详情板左边缘 x={} 压在第 {slot} 栏（右边缘 x={limit}）上\
                     ——面板该是窗格的兄弟，不是挂在窗格里的一块",
                    panel.origin.x,
                );
                // 栏里的东西一件都不许伸进面板那一列：用户报上来的正是这个
                // （双栏时筛选框伸进去 37 px，把「详细信息」的标题压在下面）。
                //
                // 只在 900 px 以上量这一条：520 px 的双栏每栏只剩 24.5 px，
                // "栏内装得下"本身做不到（命令栏一排按钮就要 184 px），
                // 那里由 `a_split_window_this_narrow_still_has_a_way_out`
                // 守"有路可走"。面包屑也不在名单里：它靠折叠祖先段收自己，
                // 深度压不住时允许伸出栏外（栏会裁掉，用户看不到越界）。
                if width >= 900. {
                    let wall = px(limit.as_f32().min(panel.origin.x.as_f32()));
                    for id in [
                        "filter-slot",
                        "view-list",
                        "view-grid",
                        "view-columns",
                        "toggle-hidden",
                        "op-more",
                    ] {
                        let stray = right_over(window, slot, id, wall);
                        assert!(
                            stray <= 0.5,
                            "窗口 {width}：第 {slot} 栏里的 {id} 越过 x={wall} 有 {stray:.1} px\
                             ——装不下就该整块降级，不许伸进详情板那一列",
                        );
                    }
                    for index in 1usize..=3 {
                        let stray = right_over(window, slot, ("header", index), wall);
                        assert!(
                            stray <= 0.5,
                            "窗口 {width}：第 {slot} 栏的表头第 {index} 列越过 x={wall} 有 {stray:.1} px",
                        );
                    }
                }
            }
        });
    }
}

/// 栏窄到装不下时**整块降级**：列表按列让位、地址栏收起筛选框。
///
/// 判据钉在"表头此刻画了哪几列"上：降级是**整格不画**，画了（哪怕被栏
/// 裁掉）就算没降级——"该降级却没降"因此能被抓住。
/// 这一条同时钉住地址栏与命令栏两处：装不下的东西不是被裁掉半边，
/// 而是整颗不画——裁掉半边的按钮点不到，还让人以为界面坏了。
#[gpui_kit::test]
fn a_narrow_pane_gives_up_columns_instead_of_overflowing(cx: &mut TestAppContext) {
    // 窗口宽度 → 每栏宽度（去掉侧栏 210 与详情板 260 之后对半分）：
    // 1600 → 564 px、1180 → 354 px、900 → 214 px。
    // （门槛见 pane.rs 里的 NAME_COL_MIN 与 ADDRESS_BAR_FILTER_AT。）
    for (width, columns, filter, buttons) in [
        (1600.0f32, vec![0usize, 1, 2, 3], true, true),
        (1180.0, vec![0usize, 1], true, true),
        (900.0, vec![0usize], false, false),
    ] {
        let dir = tempfile::tempdir().expect("临时目录");
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
        }
        let (_session, handle) = open_session_at_width(
            cx,
            &Session {
                tabs: vec![dir.path().to_path_buf(), dir.path().to_path_buf()],
                active: 0,
                split: Some(1),
                focus_on_left: true,
                sync: false,
            },
            width,
        );
        step(cx, handle, settle);

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            assert_eq!(
                header_columns(window, 0),
                columns,
                "窗口 {width}：表头该画这几列",
            );
            let scope = window.within(("pane-slot", 0usize));
            assert_eq!(
                scope.try_find("filter-slot").is_some(),
                filter,
                "窗口 {width}：筛选框该不该在",
            );
            assert_eq!(
                scope.try_find("view-list").is_some(),
                buttons,
                "窗口 {width}：视图按钮该不该在",
            );
        });
    }
}

/// 双栏里详情板画的是**焦点那一栏**的选中项：换焦点、换选中项，它都跟着换。
///
/// 面板挂在窗口上、内容却来自窗格，所以这条钉的是"面板读的是哪一栏"——
/// `render_details` 里写死成 0 号栏、或焦点换过去时不重取，这里立刻红。
///
/// **钉不住的半条**：窗格重画之后得**把窗口叫醒**（`WFilesApp::follow_pane`）
/// 面板才会刷新，而 gpui-kit 的测试每一步交互前都 `Window::refresh()`
/// （整窗无条件重画），那句订阅即使删掉这里也是绿的。那根线为什么不能删，
/// 写在 `follow_pane` 上。
#[gpui_kit::test]
fn the_details_panel_follows_the_focused_pane(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    std::fs::write(left.join("left-file.txt"), "x").unwrap();
    std::fs::write(right.join("right-file.txt"), "x").unwrap();

    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left, right],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    // 左栏是焦点栏，选中第一行。
    step(cx, handle, |window, cx| {
        window.press("down", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(detail_name(window), "left-file.txt");
    });

    // Tab 把焦点挪到右栏，在那边选一行：面板该换成右栏那个。
    step(cx, handle, |window, cx| {
        window.press("tab", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("down", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            detail_name(window),
            "right-file.txt",
            "焦点换到右栏、那边也选了文件，面板还写着左栏那个",
        );
    });

    // 挪回去也要跟着回来。
    step(cx, handle, |window, cx| {
        window.press("tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            detail_name(window),
            "left-file.txt",
            "焦点挪回左栏，面板该显示左栏选中的那个",
        );
    });
}

/// 520 px 的双栏：两侧固定边框加起来 470 px，分栏之后每栏只剩 25 px——
/// 那一栏里什么也放不下。**要求本身做不到**，所以这条测的是"有路可走"：
/// 把两侧都收掉，每栏回到 235 px，两栏都还能列文件、按钮都还在栏内。
///
/// （"25 px 的栏不重叠"由 `the_details_panel_never_overlaps_a_pane` 守着：
/// 那一栏里的东西被裁掉，但绝不画到面板上。）
#[gpui_kit::test]
fn a_split_window_this_narrow_still_has_a_way_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    for i in 0..30 {
        std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").expect("造文件");
    }
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 520.);
    step(cx, handle, settle);
    // 分栏、收侧栏、收详情板各走一步：同一个 step 里连按两个键，
    // 第一个触发的重画会把第二个键吞掉。
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-d", cx);
    });
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-b", cx);
    });
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-o", cx);
    });
    step(cx, handle, settle);

    step_value(cx, handle, |window, cx| {
        settle(window, cx);
        for slot in 0usize..2 {
            let pane = window.try_find(("pane-slot", slot)).expect("栏位").bounds();
            assert!(
                pane.size.width >= px(230.),
                "520 px 的窗口收掉两侧之后每栏该有 235 px 左右，第 {slot} 栏实测 {:?}",
                pane.size.width,
            );
            // 栏里看得见文件（空目录/被挤没了都算这条没过）。
            assert!(
                !row_names_in(window, slot).is_empty(),
                "第 {slot} 栏里一个文件名都没有——这一栏被挤没了",
            );
            let limit = pane_right(window, slot);
            for id in ["op-new-folder", "op-paste", "op-more"] {
                let stray = right_over(window, slot, id, limit);
                assert!(
                    stray <= 0.5,
                    "第 {slot} 栏里的 {id} 越出栏右边缘 {stray:.1} px",
                );
            }
            let stray = right_over(window, slot, ("header", 0usize), limit);
            assert!(
                stray <= 0.5,
                "第 {slot} 栏表头的「名称」越出栏右边缘 {stray:.1} px",
            );
        }
    });
}

/// 长文件名也不能把状态栏撑出窗口。
///
/// 撤销按钮的标签是 `describe()` 拼出来的完整描述（"重命名 X 为 Y"，
/// 两个**完整**文件名都在里面）。而 `Button` 的根节点写死
/// `flex_shrink_0`，在 flex 容器里不会收缩——实测两个 60 字的中文名
/// 能把按钮撑到 1278 px，装在 1180 px 的窗口里右边缘到 1338 px，
/// 整个状态栏被顶出屏幕。
///
/// 判据是**撤销按钮的右边缘不超过窗口右边界**。顺带看一眼 tooltip：
/// 完整描述还在那儿（无障碍名同理），标签只是不再读全。
#[gpui_kit::test]
fn a_long_undo_label_cannot_push_the_status_bar_off_screen(cx: &mut TestAppContext) {
    for width in [1180.0f32, 900.0, 700.0] {
        let dir = tempfile::tempdir().expect("临时目录");
        let long_name = format!("{}.txt", "甲".repeat(60));
        std::fs::write(dir.path().join(&long_name), "x").expect("造文件");
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);

        // 选中 → F2 重命名 → 回车。两次重命名才造得出"重命名 X 为 Y"。
        step(cx, handle, |window, cx| {
            settle(window, cx);
            window.press("cmd-a", cx);
            settle(window, cx);
            window.press("f2", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.input("second-name", cx);
            window.press("enter", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("f2", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.input("third-name", cx);
            window.press("enter", cx);
            settle(window, cx);
        });

        let undo = step_value(cx, handle, |window, _| {
            window.try_find("undo").map(|element| element.bounds())
        })
        .unwrap_or_else(|| panic!("窗口 {width}：状态栏上找不到撤销按钮"));
        assert!(
            undo.origin.x + undo.size.width <= px(width),
            "窗口 {width}：撤销按钮右边缘在 x={} px（宽 {}），超出窗口——\
             长文件名的撤销描述把状态栏顶出去了",
            undo.origin.x + undo.size.width,
            undo.size.width,
        );
    }
}

// ---------------------------------------------------------------------------
// 浮层：对话框与命令面板在窄窗口里也必须完整可用
// ---------------------------------------------------------------------------

/// 临时目录里摆八个文件，够撑起列表也够触发批量重命名。
fn overlay_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("临时目录");
    for i in 0..8 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), "x").expect("造文件");
    }
    dir
}

/// 把某个元素量出来，顺带把它的右边缘算好——越界判据关心的就是这条边。
fn bounds_of(window: &mut gpui_kit::Window, id: &str, width: f32, what: &str) -> Bounds<Pixels> {
    window
        .try_find(id.to_string())
        .map(|element| element.bounds())
        .unwrap_or_else(|| panic!("窗口 {width}：找不到{what}（element id 变了？）"))
}

/// 每一层浮层都得留在窗口里。
///
/// 判据是**四条边都在窗口内**——只查右边不够：卡片居中放置，窄到超过
/// 遮罩宽度时是**左右同时**溢出（实测批量重命名那张620px 的卡在 520px
/// 窗口里 `x = -50`），只查右边会漏掉左半边按钮点不到的情况。
fn assert_inside_window(bounds: Bounds<Pixels>, width: f32, what: &str) {
    let right = bounds.origin.x + bounds.size.width;
    assert!(
        bounds.origin.x >= px(0.) && right <= px(width),
        "{what} 左 {width}px、右 {}px，越出窗口 {}px——\
         浮层宽度是写死的，装不下时该退到可用宽度，而不是被裁掉一半",
        right,
        bounds.origin.x,
    );
}

/// 对话框卡片在窄窗口里必须完整可见，页脚按钮必须点得到。
///
/// 三类对话框都在同一张卡上（[`WFilesApp::overlay_card`]），所以测三类：
/// 420px 的重命名框、460px 的连接框、620px 的批量重命名框——620 是最宽的，
/// 也是最容易把 520px 的窗口撑破的那一张。
#[gpui_kit::test]
fn every_dialog_stays_inside_a_narrow_window(cx: &mut TestAppContext) {
    for width in [1180.0f32, 700.0, 520.0] {
        // 重命名框：420px
        {
            let dir = overlay_fixture();
            let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
            step(cx, handle, settle);
            step(cx, handle, |window, cx| {
                window.press("down", cx);
                settle(window, cx);
            });
            step(cx, handle, |window, cx| {
                window.press("f2", cx);
                settle(window, cx);
            });
            step_value(cx, handle, |window, cx| {
                settle(window, cx);
                assert_inside_window(
                    bounds_of(window, "overlay-card", width, "重命名对话框"),
                    width,
                    "重命名对话框",
                );
                let ok = bounds_of(window, "prompt-ok", width, "重命名对话框的确认按钮");
                assert!(
                    ok.origin.x >= px(0.) && ok.origin.x + ok.size.width <= px(width),
                    "确认按钮左 {}px、右 {}px（窗口 {width}px）",
                    ok.origin.x,
                    ok.origin.x + ok.size.width,
                );
            });
        }
        // 连接框：460px
        {
            let dir = overlay_fixture();
            let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
            step(cx, handle, settle);
            step(cx, handle, |window, cx| {
                window.press("cmd-p", cx);
                settle(window, cx);
            });
            step(cx, handle, |window, cx| {
                window.input("连接远程服务器", cx);
                settle(window, cx);
            });
            step(cx, handle, |window, cx| {
                window.click("app:connect", cx);
                settle(window, cx);
            });
            step_value(cx, handle, |window, cx| {
                settle(window, cx);
                assert_inside_window(
                    bounds_of(window, "overlay-card", width, "连接对话框"),
                    width,
                    "连接对话框",
                );
                assert_inside_window(
                    bounds_of(window, "connect-ok", width, "连接对话框的连接按钮"),
                    width,
                    "连接按钮",
                );
            });
        }
        // 批量重命名框：620px，最宽的那一张
        {
            let dir = overlay_fixture();
            let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
            step(cx, handle, settle);
            step(cx, handle, |window, cx| {
                window.press("cmd-a", cx);
                settle(window, cx);
            });
            step(cx, handle, |window, cx| {
                window.press("cmd-shift-r", cx);
                settle(window, cx);
            });
            step_value(cx, handle, |window, cx| {
                settle(window, cx);
                assert_inside_window(
                    bounds_of(window, "overlay-card", width, "批量重命名对话框"),
                    width,
                    "批量重命名对话框",
                );
                assert_inside_window(
                    bounds_of(window, "bulk-ok", width, "批量重命名对话框的执行按钮"),
                    width,
                    "执行重命名按钮",
                );
            });
        }
        // 设置卡：560px，四行选项 + 一排按钮（最宽的一张卡之外的第二张）
        {
            let dir = overlay_fixture();
            let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
            step(cx, handle, settle);
            step(cx, handle, |window, cx| {
                window.press("cmd-,", cx);
                settle(window, cx);
            });
            step_value(cx, handle, |window, cx| {
                settle(window, cx);
                assert_inside_window(
                    bounds_of(window, "overlay-card", width, "设置对话框"),
                    width,
                    "设置对话框",
                );
                assert_inside_window(
                    bounds_of(window, "settings-done", width, "设置对话框的完成按钮"),
                    width,
                    "完成按钮",
                );
            });
        }
    }
}

/// 命令面板也得跟着窗口收窄。
///
/// 它不走 `overlay_card`（自己一套顶部落下的布局），但同一条约束：600px
/// 写死的宽度在 520px 的窗口里会溢出，而溢出的正好是底部"Esc 关闭"那行提示
/// ——那行是新手唯一能看见的键位说明。
#[gpui_kit::test]
fn the_command_palette_stays_inside_a_narrow_window(cx: &mut TestAppContext) {
    for width in [1180.0f32, 700.0, 520.0] {
        let dir = overlay_fixture();
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
        step(cx, handle, settle);
        step(cx, handle, |window, cx| {
            window.press("cmd-p", cx);
            settle(window, cx);
        });

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            assert_inside_window(
                bounds_of(window, "palette-card", width, "命令面板"),
                width,
                "命令面板",
            );
        });
    }
}

/// 对话框的正文自己滚，页脚留在卡片底部。
///
/// 这条守的是"内容再多，按钮也不会掉出窗口"：正文那一段带
/// `overflow_y_scroll`，页脚是它的兄弟而不是子节点，所以正文滚多长都推不动
/// 页脚。反过来（把按钮塞进滚动区里）滚到底之前就点不到它。
#[gpui_kit::test]
fn the_dialog_footer_survives_a_scrolling_body(cx: &mut TestAppContext) {
    let dir = overlay_fixture();
    // 批量重命名是唯一带滚动列表的对话框，用它验。
    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 520.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-r", cx);
        settle(window, cx);
    });

    step_value(cx, handle, |window, cx| {
        settle(window, cx);
        let card = bounds_of(window, "overlay-card", 520., "批量重命名对话框");
        let body = bounds_of(window, "overlay-body", 520., "对话框正文区");
        let ok = bounds_of(window, "bulk-ok", 520., "执行重命名按钮");

        // 页脚在卡片的下半部分：正文在上、按钮在下，两条不重叠。
        assert!(
            ok.origin.y > body.origin.y,
            "执行按钮（y={}）该在正文区（y={}，高 {}）下方——\
             按钮在正文里的话，正文一长就被滚走了",
            ok.origin.y,
            body.origin.y,
            body.size.height,
        );
        // 正文与页脚都还在卡片里。
        assert!(
            body.origin.y + body.size.height <= card.origin.y + card.size.height,
            "正文区底边 {}px 越出卡片底边 {}px",
            body.origin.y + body.size.height,
            card.origin.y + card.size.height,
        );
        assert!(
            ok.origin.y + ok.size.height <= card.origin.y + card.size.height,
            "执行按钮底边 {}px 越出卡片底边 {}px",
            ok.origin.y + ok.size.height,
            card.origin.y + card.size.height,
        );
    });
}

/// 批量重命名的五个输入框都得拿到自己声明的宽度。
///
/// 这条守的是"别再挤成一行"：一行摆下 260+120+120+80+70 加四个间隙是
/// 698 px，而 620 px 的卡片只剩 580 px 内容宽——挤在一行时**五个框全都在缩**
/// （实测 1180 px 的宽窗口下模板 260→212、步长 70→57，跟窗口宽窄毫无关系）。
/// 拆成两行之后，每一行的需求都在可用宽度之内。
#[gpui_kit::test]
fn the_bulk_rename_fields_get_the_width_they_ask_for(cx: &mut TestAppContext) {
    for width in [1180.0f32, 520.0] {
        let dir = overlay_fixture();
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
        step(cx, handle, settle);
        step(cx, handle, |window, cx| {
            window.press("cmd-a", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("cmd-shift-r", cx);
            settle(window, cx);
        });

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            for (id, want) in [
                ("bulk-find", 120.),
                ("bulk-replace", 120.),
                ("bulk-start", 80.),
                ("bulk-step", 70.),
            ] {
                let got = bounds_of(window, id, width, id).size.width;
                assert!(
                    got >= px(want),
                    "窗口 {width}：{id} 要 {want} px，实得 {got} px——\
                     五个框又被挤回一行了",
                );
            }
            // 模板独占一行：它下面那行（查找/替换/起始/步长）的顶边必须在它底下。
            let template = bounds_of(window, "bulk-template", width, "名字模板输入框");
            let find = bounds_of(window, "bulk-find", width, "查找输入框");
            assert!(
                find.origin.y > template.origin.y,
                "窗口 {width}：查找框（y={}）该在模板框（y={}）下方——\
                 模板要占一整行，那是它唯一能打得下完整模板串的地方",
                find.origin.y,
                template.origin.y,
            );
            // 模板是撑开的那一个：比它下面任何一格都宽。
            assert!(
                template.size.width > find.size.width,
                "窗口 {width}：模板框 {}px 该比参数框 {}px 宽",
                template.size.width,
                find.size.width,
            );
        });
    }
}

/// 批量重命名的预览有行数上限，而且要把话说清楚。
///
/// 预览列表原来整份建出来（它不是虚拟列表）：选中几百项就是每帧几百行，
/// 十万项时这个对话框直接卡死。现在只画前 `PREVIEW_ROWS` 行，并在下面
/// 注明"只列了前几项、确认后全部生效"。
///
/// 这条钉住两件事：第 201 行不再渲染；那行说明确实在，而且报的是真实总数。
#[gpui_kit::test]
fn the_bulk_rename_preview_is_capped_and_says_so(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..205 {
        std::fs::write(dir.path().join(format!("f{index:03}.txt")), "x").unwrap();
    }
    let (_session, handle) = open_in(cx, dir.path());
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("cmd-a", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-r", cx);
        settle(window, cx);
    });

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find(("bulk-row", 199usize)).is_some(),
            "第 200 行该在"
        );
        assert!(
            window.try_find(("bulk-row", 200usize)).is_none(),
            "第 201 行不该画出来（预览有上限）"
        );
        let note = window
            .try_find("bulk-preview-note")
            .and_then(|node| node.label().map(|label| label.to_string()));
        let note = note.expect("截断时该有一行说明");
        assert!(
            note.contains("200") && note.contains("205"),
            "说明里要交代「列了 200、共 205」：{note}"
        );
    });
}

/// 色点不能让某些行的文件名单独错开。
///
/// 原来色点只在"这个文件被标记过"时才插进行里，于是那一行的名字比旁边所有行
/// 右移 8 px（点宽）加一个 12 px 的 gap = 20 px。界面上看着只是"这一行的字没对齐"，
/// 不量坐标发现不了；而一行一行地对不齐，整列名字就成了波浪。
///
/// 现在色点槽**整列常驻**：标了色的画真颜色，没标记的画全透明。
#[gpui_kit::test]
fn the_mark_dot_does_not_shift_the_names_of_unmarked_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    for name in ["a-marked.txt", "b-plain.txt", "c-plain2.txt"] {
        std::fs::write(dir.path().join(name), "x").expect("造文件");
    }
    let (_isolation, handle) = open_in(cx, dir.path());
    step(cx, handle, settle);

    // 标记第一行。
    step(cx, handle, |window, cx| {
        window.press("down", cx);
        settle(window, cx);
        window.press("cmd-alt-m", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("mark-color-red", cx);
        settle(window, cx);
        window.press("enter", cx);
    });

    step_value(cx, handle, |window, cx| {
        settle(window, cx);
        // 三行的色点槽左边缘必须一致——它紧挨着文件名的起排处。
        let xs: Vec<Pixels> = (0..3usize)
            .map(|position| {
                window
                    .try_find(("row-mark", position))
                    .unwrap_or_else(|| panic!("找不到第 {position} 行的色点槽"))
                    .bounds()
                    .origin
                    .x
            })
            .collect();
        assert!(
            xs.windows(2).all(|pair| pair[0] == pair[1]),
            "三行的色点槽左边缘分别是 {xs:?}——有一行被多推了一把，\
             标记色点还在按需插入",
        );
    });
}

/// 表头的「名称」要跟下面所有行的文件名列起排。
///
/// 这条和上一条是一件事的两头：行里多了个 8 px 的色点槽，表头不补上同一个槽，
/// "名称"两个字就比下面的文件名右移 20 px（槽宽 + 一个 gap）。
#[gpui_kit::test]
fn the_name_header_lines_up_with_the_file_names(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    for name in ["a-marked.txt", "b-plain.txt", "c-plain2.txt"] {
        std::fs::write(dir.path().join(name), "x").expect("造文件");
    }
    let (_isolation, handle) = open_in(cx, dir.path());
    step(cx, handle, settle);
    // 标记第一行：色点槽存在，才量得到它跟表头的关系。
    step(cx, handle, |window, cx| {
        window.press("down", cx);
        settle(window, cx);
        window.press("cmd-alt-m", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("mark-color-red", cx);
        settle(window, cx);
        window.press("enter", cx);
    });

    step_value(cx, handle, |window, cx| {
        settle(window, cx);
        let header = window.try_find(("header", 0usize)).expect("名称列表头");
        let mark = window
            .try_find(("row-mark", 0usize))
            .expect("第一行的色点槽");
        // 名字紧挨着色点槽（中间一个 12 px 的 gap）。
        let gap = header.bounds().origin.x - (mark.bounds().origin.x + mark.bounds().size.width);
        assert!(
            (gap - px(12.)).abs() <= px(1.),
            "表头「名称」左边缘 {}px、色点槽右边缘 {}px，差 {gap}px\
             （该是12 px 的间隙）——表头少留了色点槽那一段",
            header.bounds().origin.x,
            mark.bounds().origin.x + mark.bounds().size.width,
        );
    });
}

/// 路径很深时，面包屑把老段收进「…」，当前目录必须留在容器里。
///
/// 实测（520px 窗口、收掉两侧、6 层深且目录名都不短）：
/// 改之前最后一段落在 x=582，而容器右边缘才372——**用户最需要看的"我在哪"
/// 在屏幕外**。祖先段压到最小也救不回来：4 段保底 28px 加箭头是 160px，
/// 而容器只有 216px，剩下的位置装不下超长目录名。
///
/// 这条守的是"折叠"这个动作本身：老段收进「…」、当前目录留在容器内、
/// 而且「…」能点（它连着上一级，不是死装饰）。
#[gpui_kit::test]
fn a_deep_path_folds_into_an_ellipsis(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    // 六层深，名字都不短——比任何一档窗口的面包屑都长。
    let mut deep = dir.path().to_path_buf();
    for i in 0..5 {
        deep = deep.join(format!("第{i}层-{}", "很长".repeat(20)));
        std::fs::create_dir_all(&deep).expect("造深层目录");
    }
    std::fs::write(deep.join("leaf.txt"), "x").expect("造文件");

    for width in [1180.0f32, 700.0, 520.0] {
        let (_isolation, handle) = open_in_at_width(cx, &deep, width);
        step(cx, handle, settle);
        // 收掉两侧，把宽度让给面包屑——这一条测的是"内容区够宽时它对不对"。
        step(cx, handle, |window, cx| {
            window.press("cmd-b", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("cmd-alt-o", cx);
            settle(window, cx);
        });

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            let bar = window.try_find("breadcrumb").expect("面包屑").bounds();
            let right = bar.origin.x + bar.size.width;

            // 当前目录（最后一段）必须在容器内。
            let current = deepest_crumb(window).expect("至少要有一段路径");
            assert!(
                current.origin.x >= bar.origin.x && current.origin.x + current.size.width <= right,
                "窗口 {width}：当前目录那一段落在 x={}~{}，容器是 {}~{right}——\
                 老段没折叠进去，当前目录被挤出屏幕了",
                current.origin.x,
                current.origin.x + current.size.width,
                bar.origin.x,
            );

            // 老段收进了「…」，它得能点：点一下回到上一级。
            assert!(
                window.try_find("crumb-more").is_some(),
                "窗口 {width}：路径这么深却没有「…」——老段全留在屏幕上，\
                 当前目录必然被挤出去",
            );
        });

        // 「…」真能往上走一级：当前目录那一段会变窄（它不再是那段超长的
        // 目录名了）。序号不能写死成"第几层"——路径里还带着 macOS 的
        // /var/folders 与临时目录，先把当前实际的末段序号读出来。
        let before = step_value(cx, handle, |window, cx| {
            settle(window, cx);
            deepest_crumb(window).map(|at| at.size.width)
        });
        step(cx, handle, |window, cx| {
            settle(window, cx);
            window.click("crumb-more", cx);
            settle(window, cx);
        });
        let after = step_value(cx, handle, |window, cx| {
            settle(window, cx);
            deepest_crumb(window).map(|at| at.size.width)
        });
        assert_ne!(
            before, after,
            "窗口 {width}：点了「…」什么都没变——它连着上一级，不是死装饰"
        );
    }
}

/// 面包屑的段落不能整体跑到容器左边之外。
///
/// 原来每一段都是 `flex_shrink_0`：整条路径按固有宽度排开，装不下时
/// `justify_end` 把**整行**推到容器左边界之外（实测 1180px 窗口里第一段落在
/// x=127，而容器左边缘在 318）。"从左边裁掉"听起来像 `overflow_hidden` 就能办，
/// 实际上溢出的是行本身，裁只裁得掉已经跑出去的部分。
#[gpui_kit::test]
fn no_breadcrumb_escapes_the_address_bar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut deep = dir.path().to_path_buf();
    for i in 0..4 {
        deep = deep.join(format!("第{i}层-{}", "很长".repeat(20)));
        std::fs::create_dir_all(&deep).expect("造深层目录");
    }
    std::fs::write(deep.join("leaf.txt"), "x").expect("造文件");

    for width in [1180.0f32, 700.0, 520.0] {
        let (_isolation, handle) = open_in_at_width(cx, &deep, width);
        step(cx, handle, settle);
        step(cx, handle, |window, cx| {
            window.press("cmd-b", cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("cmd-alt-o", cx);
            settle(window, cx);
        });

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            let bar = window.try_find("breadcrumb").expect("面包屑").bounds();
            for index in 0..8usize {
                let Some(crumb) = window.try_find(("crumb", index)) else {
                    continue;
                };
                let at = crumb.bounds();
                assert!(
                    at.origin.x >= bar.origin.x - px(1.)
                        && at.origin.x + at.size.width <= bar.origin.x + bar.size.width + px(1.),
                    "窗口 {width}：crumb{index} 落在 x={}~{}，容器 {}~{}——\
                     有段落跑到地址栏外面去了",
                    at.origin.x,
                    at.origin.x + at.size.width,
                    bar.origin.x,
                    bar.origin.x + bar.size.width,
                );
            }
        });
    }
}

/// 路径从**左**起排：最左边那一段贴着容器左内边距，左边不留空白。
///
/// 用户 2026-10-18 报的："地址栏正常不都居左吗，你这咋都在最右边，左边全是
/// 一大片空白"。那会儿这里是 `justify_end`，还配了一句"路径从右边起排是
/// Explorer 的习惯"——那句话是编的：Finder / Explorer / GNOME Files / Dolphin
/// 全是左起。
///
/// 判据落在**左边那段空白有多宽**上：右对齐时它等于"容器宽 − 路径宽"
/// （1180 px 窗口里两百多像素，一眼就是没对齐），左起时它只剩容器那 8 px
/// 内边距。跟上面 `no_breadcrumb_escapes_the_address_bar` 是一对：那条管
/// 右边不越界，这条管左边不塌空。
#[gpui_kit::test]
fn the_path_starts_at_the_left_edge(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    // 短名的目标目录：内容比容器窄，"左边剩多少空白"才量得出来。
    // 拿临时目录自己当目标的话，那串随机名把三段撑得跟容器差不多宽，
    // 两种对齐都在左边量出小值——测试就废了（这条是被红检打回来改的）。
    let short = dir.path().join("sub");
    std::fs::create_dir_all(&short).expect("造目录");
    let (_isolation, handle) = open_in(cx, &short);
    step(cx, handle, settle);
    // 收掉两侧，把宽度让给面包屑——跟上面那条一样。
    step(cx, handle, |window, cx| {
        window.press("cmd-b", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-alt-o", cx);
        settle(window, cx);
    });

    let (gap, bar_width) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        let bar = window.try_find("breadcrumb").expect("面包屑").bounds();

        // 最左边那一段：折叠时是「…」，没折叠时是序号最小的 crumb。
        let mut leftmost = px(f32::MAX);
        for index in 0..12usize {
            if let Some(crumb) = window.try_find(("crumb", index)) {
                let at = crumb.bounds().origin.x;
                if at < leftmost {
                    leftmost = at;
                }
            }
        }
        if let Some(folded) = window.try_find("crumb-folded") {
            let at = folded.bounds().origin.x;
            if at < leftmost {
                leftmost = at;
            }
        }
        assert!(leftmost < px(f32::MAX), "至少要有一段路径");

        (leftmost - bar.origin.x, bar.size.width)
    });

    assert!(
        gap <= px(16.),
        "路径没贴着左边：左边空了 {gap}（容器宽 {bar_width}）——\
         居左那一路下这里只剩容器的内边距（8 px）"
    );
}

/// 地址栏点一下就能改：进门有三条路（点空白 / 点当前那一段 / `cmd-l`），
/// 出门有两条（回车前往、Esc 放下）。
#[gpui_kit::test]
fn the_address_bar_becomes_an_editable_path_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    // 造两行：下面要验"收掉输入框之后窗格级的按键还灵不灵"，得有两行
    // 才看得出**这一次**按键真的生效了（只有一行的话，上一轮的选中
    // 会伪装成这一轮通过）。
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    std::fs::write(dir.path().join("b.txt"), "x").expect("造文件");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);
    let before = cx.update(|cx| app.read(cx).tab_paths(cx));

    // ① 点面包屑右边那条空白。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-blank").is_some(),
            "面包屑右边该留有一条可点的空白",
        );
        window.click("location-blank", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_some(),
            "点空白之后该出现路径输入框",
        );
        assert!(
            deepest_crumb(window).is_none(),
            "编辑时面包屑该整个让位，不是叠在上面",
        );
    });

    // ② 一个字都不改，直接回车：收起输入框、留在原地。
    //    这一条顺带钉住"预填的是**真实路径**"——填进去的要是折叠后的那串
    //    （「…」），回车会得到"找不到"，输入框根本不会收。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_none(),
            "回车之后该收起输入框",
        );
        assert!(deepest_crumb(window).is_some(), "面包屑该回来",);
    });
    assert_eq!(
        cx.update(|cx| app.read(cx).tab_paths(cx)),
        before,
        "一个字没改就回车，该还在原来那个目录",
    );

    // 键盘必须有人接手：地址栏那一块是**当场从渲染树里撤掉**的，焦点要是
    // 还挂在它身上，窗格级的快捷键会整片变哑——不报错、不留痕（AGENTS.md
    // 记着这个坑）。判据只能是"再按一个键看看灵不灵"：焦点在不在渲染树里，
    // 从界面上看不出来。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("down", cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["a.txt".to_string()],
        "回车收掉输入框之后，窗格级的按键该还灵（↓ 得能选中第一行）",
    );

    // ③ `cmd-l` 进门、Esc 出门。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-l", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_some(),
            "cmd-l 该进编辑",
        );
        window.press("escape", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_none(),
            "Esc 该收起输入框",
        );
        // Esc 这条路，把键盘收回来的**只有** `end_edit_location` 那一句
        // （回车那条路上 `navigate_to` 顺手也会收）。所以这一条探针才是
        // 它的判据：再按一次 ↓，选中该从 a.txt 挪到 b.txt。
        window.press("down", cx);
    });
    assert_eq!(
        selected_names(cx, &app),
        vec!["b.txt".to_string()],
        "Esc 收起输入框之后，窗格级的按键该还灵（↓ 得能挪到第二行）",
    );

    // ④ 点**当前那一段**也进编辑（Explorer 就是这个行为）。
    let current = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        deepest_crumb_index(window)
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("crumb", current), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_some(),
            "点当前那一段该进编辑",
        );
    });
}

/// 敲一条路径回车：真的过去。
#[gpui_kit::test]
fn typing_a_path_goes_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    // 名字里带空格：路径是**照字面**走的，谁也不许在中间替用户拆词。
    let target = dir.path().join("目标 目录");
    std::fs::create_dir(&target).expect("造目录");
    // 另造一个排在最前头的兄弟目录，一会儿先把它选中（见下面那段）。
    let other = dir.path().join("aa-另一条");
    std::fs::create_dir(&other).expect("造目录");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);

    // 先选中一行：回车要是被窗格那条"打开选中项"接走了，下面就会落在
    // `aa-另一条` 上，而不是我敲的那条。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    assert_eq!(selected_names(cx, &app), vec!["aa-另一条".to_string()]);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-l", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 输入框里现在是全选的当前路径：直接敲就是把它整条换掉。
        window.input(&target.to_string_lossy(), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_none(),
            "到了就该收起输入框",
        );
    });

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(live.len(), 1, "还是那一栏");
    assert!(
        same_dir(&live[0], &target),
        "该去我敲的那条 {}，实际是 {}——回车只该前往地址栏里的路径，\
         不该顺手打开选中的那一行",
        target.display(),
        live[0].display(),
    );
}

/// 敲一个**文件**的路径回车：去它所在的目录，并把它选中。
#[gpui_kit::test]
fn typing_a_file_reveals_it_in_its_folder(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("报告.txt"), "x").expect("造文件");
    let file = dir.path().join("报告.txt");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-l", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.input(&file.to_string_lossy(), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, settle);

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        same_dir(&live[0], dir.path()),
        "该停在文件所在的那个目录，实际是 {}",
        live[0].display(),
    );
    assert_eq!(
        selected_names(cx, &app),
        vec!["报告.txt".to_string()],
        "该把那个文件选中——地址栏说的是\"带我去看它在哪儿\"",
    );
}

/// 路径不存在：不导航、**不收起输入框**，状态栏把话说明白。
#[gpui_kit::test]
fn a_path_that_does_not_exist_keeps_the_field_open(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);
    let before = cx.update(|cx| app.read(cx).tab_paths(cx));

    let missing = dir.path().join("并没有这个目录");
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-l", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.input(&missing.to_string_lossy(), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });

    let status = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_some(),
            "敲错了不该把输入框收掉——用户还要在那上面改字",
        );
        a11y_label_at(window, "pane-status")
    });
    let needle = missing.to_string_lossy().into_owned();
    assert!(
        status.contains(&needle),
        "状态栏该点出是哪条路径没找到（{needle}），实际是 {status:?}",
    );
    assert_eq!(
        cx.update(|cx| app.read(cx).tab_paths(cx)),
        before,
        "路径不存在就不许导航",
    );
}

/// 远程位置的写法（`dav://连接/子路径`）粘进地址栏：去那儿，
/// 而不是被当成一条相对路径说「找不到」。
///
/// 用了一个不存在的连接 id，是**故意的**：`Remotes::client` 认不出这个 id
/// 就到此为止（不会去开后台任务），测试因此是确定的——等真起了服务器
/// 那套再来测这一条，就变成在测网络了。
#[gpui_kit::test]
fn a_remote_location_can_be_typed_into_the_address_bar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);

    let location = crate::remote::location("没接过的连接", "子目录");
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-l", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.input(&location.to_string_lossy(), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_none(),
            "认得的远程位置就该走，不该停在编辑状态",
        );
    });

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert_eq!(
        live,
        vec![location],
        "该落到那条远程位置上，而不是「找不到」",
    );
}
///
/// 这是"可点的空白做成兄弟元素、不挂在面包屑容器上"的判据：容器级的点击
/// 处理器会被子元素连带上（事件先给子、再往上冒），一次点击就既导航
/// 又进编辑。这条测试在换回容器级写法时**必然**变红。
#[gpui_kit::test]
fn clicking_an_ancestor_crumb_navigates_without_editing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    let deep = dir.path().join("子目录");
    std::fs::create_dir(&deep).expect("造目录");
    let (_isolation, handle, app) = open_in_with_app(cx, &deep);
    step(cx, handle, settle);

    // 屏幕上最左边那一段——它是某一级祖先，点它该往上走。
    // （深路径上它**不是**根：屏幕上只留最后 `MAX_CRUMBS` 段，再往上的
    // 老段收进了「…」。所以断言只要求"走到了某一级祖先"。）
    let first = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (0..24usize)
            .find(|index| window.try_find(("crumb", *index)).is_some())
            .expect("该有一段面包屑")
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("crumb", first), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("location-input").is_none(),
            "点祖先段是导航，不该顺手进编辑",
        );
    });

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        !same_dir(&live[0], &deep) && deep.starts_with(&live[0]),
        "该往上走到某一级祖先，实际是 {}",
        live[0].display(),
    );
}

/// 面包屑里序号最大的那一段，也就是当前目录。
///
/// 序号不能写死成"第几层"：临时目录本身带着 `/var/folders/...` 与随机串，
/// 真实段数比测试自己造出来的层数多好几段。
///
/// **从后往前找，不是 `filter_map(…).last()`**：后者会把 0..24 全扫一遍
/// 才返回最后一个，clippy 也会报它（DoubleEndedIterator 上的 `last`）。
/// 序号是连续的，从 23 往下第一个命中的就是最大的那个 —— 同样一个结果，
/// 但一般两三次就返回了。
fn deepest_crumb(window: &mut gpui_kit::Window) -> Option<Bounds<Pixels>> {
    (0..24usize)
        .rev()
        .find_map(|index| window.try_find(("crumb", index)).map(|it| it.bounds()))
}

/// 当前目录那一段的**序号**（屏幕上看最后一段面包屑）。
///
/// [`deepest_crumb`] 给的是坐标，要按着它点就得知道序号。
fn deepest_crumb_index(window: &mut gpui_kit::Window) -> usize {
    (0..24usize)
        .rev()
        .find(|index| window.try_find(("crumb", *index)).is_some())
        .expect("该有一段面包屑")
}

/// Quick Look 的遮罩要盖住**整个窗口**，不只是窗格那一块。
///
/// 它是"看一眼整个文件"，不是某一栏的事。挂在窗格里的时候遮罩从侧栏右缘
/// 开始：实测 1180 px 的窗口里遮罩 `x = 210`，标签栏和侧栏都亮着，浮层被框在
/// 窗格中间；520 px 的窗口里窗格只剩 310 px 宽，那张 720 px 的卡片根本没法看。
///
/// 判据是遮罩的四条边都贴着窗口——跟上面那几条对话框测试同一个道理，
/// 只查"卡片没越界"不够：卡片在窗格内也算没越界，但遮罩该盖的没盖。
#[gpui_kit::test]
fn quick_look_covers_the_whole_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("note.txt"), "hello").expect("造文件");

    for width in [1180.0f32, 700.0, 520.0] {
        let (_isolation, handle) = open_in_at_width(cx, dir.path(), width);
        step(cx, handle, settle);
        step(cx, handle, |window, cx| {
            window.press("down", cx);
            settle(window, cx);
            window.press("space", cx);
            settle(window, cx);
        });

        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            let veil = window
                .try_find("quick-look")
                .unwrap_or_else(|| panic!("窗口 {width}：按空格该弹出 Quick Look"))
                .bounds();
            assert!(
                veil.origin.x <= px(1.)
                    && veil.origin.y <= px(1.)
                    && veil.origin.x + veil.size.width >= px(width - 1.),
                "窗口 {width}：Quick Look 遮罩是 x={}~{}——\
                 侧栏与标签栏没被盖住，浮层被框在窗格里",
                veil.origin.x,
                veil.origin.x + veil.size.width,
            );
            // 卡片自己也得留在窗口里。
            let card = window
                .try_find("quick-look-card")
                .expect("Quick Look 卡片")
                .bounds();
            assert_inside_window(card, width, "Quick Look 卡片");
        });
    }
}

/// Quick Look 在窄窗口里也要能用：卡片退到窗口宽度，正文自己滚。
///
/// 720px 的卡片在 520px 的窗口里装不下，而 Quick Look 恰恰是最需要看全的时候
/// ——文件名都已经截短了，再看不到正文等于什么都没说。
#[gpui_kit::test]
fn quick_look_fits_a_narrow_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    // 几百行：够正文滚起来，量得到「关闭」按钮留在原地。
    let body = (0..400)
        .map(|i| format!("第 {i} 行"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("long.txt"), body).expect("造文件");

    let (_isolation, handle) = open_in_at_width(cx, dir.path(), 520.);
    step(cx, handle, settle);
    step(cx, handle, |window, cx| {
        window.press("down", cx);
        settle(window, cx);
        window.press("space", cx);
        settle(window, cx);
    });

    step_value(cx, handle, |window, cx| {
        settle(window, cx);
        let card = window
            .try_find("quick-look-card")
            .expect("Quick Look 卡片")
            .bounds();
        assert_inside_window(card, 520., "Quick Look 卡片");
        let close = window
            .try_find("quick-look-close")
            .expect("关闭按钮")
            .bounds();
        // 「关闭」必须在卡片内、正文之下——正文滚多长都推不动它。
        assert!(
            close.origin.y > card.origin.y
                && close.origin.y + close.size.height <= card.origin.y + card.size.height,
            "关闭按钮（y={}~{}）掉出卡片（y={}~{}）了",
            close.origin.y,
            close.origin.y + close.size.height,
            card.origin.y,
            card.origin.y + card.size.height,
        );
    });

    // 点它真能关：这一条守住「关掉谁」的判断（双栏时不能写死一栏）。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("quick-look-close", cx);
        settle(window, cx);
    });
    let gone = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("quick-look").is_none()
    });
    assert!(gone, "点了「关闭」Quick Look 还开着");
}

/// 读某个元素上的**无障碍名**。
///
/// 名字叫 `text_at` 是错的：它读不到界面上显示的字。GPUI 的测试快照只收了
/// 无障碍属性，`ElementSnapshot::value()` 的文档原话是
/// "not rendered text or pixels"，而 `Button::tooltip()` 与 `ListItem` 的
/// 无障碍名都**不进快照**（插探针量过：带 tooltip 的按钮 `label=None`，
/// 纯 div 也 `label=None`，只有显式 `accessibility_label` 的元素才有值）。
///
/// 所以验 i18n 只有一条路可走：**给要验的那几处文案补上无障碍名**。
/// 这不是为测试改代码——侧栏的品牌标语、段落标题本来就该有，
/// 读屏软件现在念不出它们。
fn a11y_label_at(window: &mut gpui_kit::Window, id: &str) -> String {
    window
        .try_find(id.to_string())
        .and_then(|element| element.label().map(|label| label.to_string()))
        .unwrap_or_default()
}

#[gpui_kit::test]
fn the_sidebar_speaks_english_when_english_is_chosen(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    // 默认中文（隔离目录里没有设置文件，而 CI 上 LANG 通常是 C/POSIX，
    // 认不出语言 → 回落中文）。
    let chinese = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "sidebar-tagline"),
            a11y_label_at(window, "sidebar-places-title"),
            a11y_label_at(window, "sidebar-local"),
        )
    });
    assert_eq!(chinese.0, "快速、清楚、可撤销");
    assert_eq!(chinese.1, "位置");
    assert_eq!(chinese.2, "本地文件系统");

    // 改语言之后**重画一遍**界面上就得是英文。
    // 这一步是这一批 i18n 唯一的真验证：查表函数返回什么我说了算，
    // 界面会不会跟着变，只有真的切一次语言才知道。
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    let english = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "sidebar-tagline"),
            a11y_label_at(window, "sidebar-places-title"),
            a11y_label_at(window, "sidebar-local"),
        )
    });
    assert_eq!(english.0, "Fast, clear, undoable");
    assert_eq!(english.1, "Places");
    assert_eq!(english.2, "Local file system");
}

/// 上一条测试留在盘上的「英文」不该改变下一条看到的界面——那次偶发失败的
/// 最小场景。
///
/// 语言是**当场落盘**的设置，而隔离目录按 worker 线程复用（同一条测试里可以
/// 重入，跨测试却是同一个目录）。要验英文本地化的那十来条测试一改语言就
/// 落了盘，同线程后面的测试再开窗时会把那份 `en` 当成"用户的选择"读回来——
/// 表现是"单跑必过、全量偶发挂"，而且一挂就是几十条跟语言不相干的文案
/// 断言，从失败信息上根本看不出是语言的事。
///
/// 挡它的地方在 [`open_app_starting`] 开窗**之前**调的
/// [`clear_leftover_locale`]：把盘上那份语言清成"没选过"。这条测试先把盘上
/// 写成英文，再看界面——修之前这一条必然挂（探针实测：侧栏读出来是
/// `Local file system`）。
#[gpui_kit::test]
fn a_leftover_settings_file_cannot_change_what_the_next_test_sees(cx: &mut TestAppContext) {
    let isolation = isolate_session();
    let mut leftover = Settings::default();
    leftover.set_locale(crate::i18n::Locale::En);
    leftover
        .save_to(&isolation.path().join("settings.json"))
        .expect("写一份上一条测试留下的设置");

    let handle = open_app(cx);
    let seen = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "sidebar-tagline"),
            a11y_label_at(window, "sidebar-local"),
        )
    });
    assert_eq!(
        (seen.0.as_str(), seen.1.as_str()),
        ("快速、清楚、可撤销", "本地文件系统"),
        "盘上留着的英文不该盖住这一条测试"
    );
}

/// 侧栏左下那颗「本地文件系统」是**能点的**：一步回到本机根。
///
/// 这颗真正的用途是从远程连接里退回来——本机根由
/// [`crate::volumes::machine_root`] 给（macOS/Linux 是 `/`，Windows 是系统盘），
/// 断言直接跟它比，别在测试里写死那个路径。
#[gpui_kit::test]
fn the_sidebar_local_row_goes_to_the_machine_root(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    let (_isolation, handle, app) = open_in_with_app(cx, dir.path());
    step(cx, handle, settle);

    let root = crate::volumes::machine_root();
    let before = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        !same_dir(&before[0], &root),
        "起点不该已经就是本机根（{}），否则这条测试什么都没验到",
        root.display(),
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("sidebar-local", cx);
    });
    step(cx, handle, settle);

    let live = cx.update(|cx| app.read(cx).tab_paths(cx));
    assert!(
        same_dir(&live[0], &root),
        "点「本地文件系统」该到 {}，实际是 {}",
        root.display(),
        live[0].display(),
    );
}

#[gpui_kit::test]
fn sidebar_place_names_follow_the_chosen_language(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    // 「个人文件夹 / 桌面 / 文稿 / 下载」这一组：它们在 `places()` 里是
    // `&'static str`，界面上是 ListItem 的 label。
    let names = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        let mut names = Vec::new();
        for index in 0usize..8 {
            let Some(item) = window.try_find(("place", index)) else {
                break;
            };
            names.push(item.label().unwrap_or_default().to_string());
        }
        names
    });

    // 一个都没有 = 侧栏压根没画出来，这条断言就不会失败——那是假的通过。
    assert!(!names.is_empty(), "侧栏一个「位置」条目都没有");
    // 家目录在 macOS 上叫 "Home"；只要全部是英文、不含中文就算过。
    assert!(
        names
            .iter()
            .all(|name| !name.chars().any(|c| c as u32 > 0x2E80)),
        "切到英文之后侧栏条目里还有中文：{names:?}"
    );
}

#[gpui_kit::test]
fn the_command_palette_switches_the_interface_language(cx: &mut TestAppContext) {
    let session = isolate_session();
    let handle = open_app(cx);

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("界面语言", cx);
    });

    // 面板里要列出两种语言 + "跟随系统"。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("locale:auto").is_some(),
            "语言列表里没有「跟随系统」"
        );
        assert!(
            window.try_find("locale:zh-Hans").is_some(),
            "语言列表里没有「简体中文」"
        );
        assert!(
            window.try_find("locale:en").is_some(),
            "语言列表里没有「English」"
        );
    });

    // 点English：面板关上，设置落盘，界面立刻变英文。
    step(cx, handle, |window, cx| {
        window.click("locale:en", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("palette-overlay").is_none(),
            "换完语言后面板应该关上"
        );
        assert_eq!(
            a11y_label_at(window, "sidebar-places-title"),
            "Places",
            "换语言之后侧栏没跟着变——设置改了但界面没重画"
        );
    });

    // 落盘：刚点完 English 时就该存下 "en"。
    //
    // 放在"改回中文"之前 —— 那一步已经把设置清空了，再读文件只会读到空串，
    // 断言会变成"清空之后确实是空的"，跟"选中的语言有落盘"毫无关系。
    let stored =
        Settings::load_from(&session.path().join("settings.json")).expect("settings.json 应该在");
    assert_eq!(
        stored.locale, "en",
        "选中的语言没写进 settings.json，实际是 {:?}",
        stored.locale
    );
    // 反过来：把语言清空，界面回到中文。
    //
    // 两条路都验过界面跟着改了，但**验不到"是谁触发的重画"**：
    // `settle()` 会无条件重画，所以摘掉 `SetLocale` 里的 `cx.notify()`
    // 测试照样过——面板路径上 `close_palette` 自己就 notify，关面板那次
    // 重画把界面刷了一遍，看起来像是语言切换生效了。
    //
    // 那句 notify 因此是冗余的，但**不能删**：面板是眼下唯一的调用方，
    // 下一个调用方（设置界面、语言菜单）不会有 close_palette 帮它重画。
    // GPUI 的测试接口给不出"这次重画是谁要的"，所以这条只能靠人记着。
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.locale = String::new());
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            a11y_label_at(window, "sidebar-places-title"),
            "位置",
            "语言改回中文之后侧栏没跟着变"
        );
    });
}

#[gpui_kit::test]
fn follow_system_language_is_a_separate_choice_from_the_current_one(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    // 先选 English：设置里存的是 "en"。
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    // 搜索词要用**此刻界面上的那种语言**：上面已经把界面切成英文了，
    // 而面板条目也已经是英文——输中文搜不到任何东西。
    // 这不是"面板搜不了中文"，是搜的就是界面上显示的字。
    step(cx, handle, |window, cx| {
        window.input("Interface language", cx);
    });

    // "跟随系统"此刻**不该**标成"当前"——当前是 English。
    //
    // 读的是 hint 而不是行的 label：行只把标题喂给 aria_label，
    // 状态标记是另一个元素（探针量过：三条的 label 都只有标题）。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("locale:auto").is_some(),
            "「跟随系统」应该在列表里"
        );
        assert_eq!(
            palette_hint(window, "locale:auto"),
            "",
            "选了 English 之后「跟随系统」还标着当前"
        );
        assert_eq!(
            palette_hint(window, "locale:en"),
            "Current",
            "选了 English 之后 English 那条才该标着当前"
        );
    });

    // 点「跟随系统」：设置里存空串，而不是把解析出来的 en 写回去。
    //
    // 这两件事必须分得开：把当前语言写回去的话，"跟随系统"就永远失效了——
    // 用户换了系统语言也不生效，而界面上还写着"跟随系统"，那是在骗人。
    step(cx, handle, |window, cx| {
        window.click("locale:auto", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(
            Settings::peek(cx, |settings| settings.locale.clone()),
            String::new(),
            "选「跟随系统」应该把设置里的语言清空"
        );
    });
}

/// 面板里某条命令标着的 hint（"当前"这类状态标记）；没标记就是空串。
///
/// 行的 `aria_label` 只有标题，hint 是另一个元素，只能单独读。
/// 而 hint 是**按需插入**的——没标记的行压根没有这个元素，
/// 所以"没标记"表现为查不到，而不是查到一个空串。
fn palette_hint(window: &mut gpui_kit::Window, command_id: &str) -> String {
    window
        .try_find(format!("palette-hint:{command_id}"))
        .and_then(|element| element.label().map(|label| label.to_string()))
        .unwrap_or_default()
}

/// 命令面板当前列出的所有条目标题。
///
/// **读的是 `WFilesApp` 里的 `palette.items` 而不是渲染树**：面板条目是
/// `open_palette` 时快照的一份数据（`self.palette.items`），之后输入只是
/// 过滤这份列表。所以问应用"你现在列了哪些"比在渲染树里翻要准，也省掉了
/// "怎么按行遍历"这个难题——面板行的 id 是命令 id，而 `.id()` 不叠加，
/// 另加一个行号 id 会把命令 id 顶掉（实测九条既有面板测试全挂）。
fn palette_titles(
    app: &gpui_kit::Entity<crate::app::WFilesApp>,
    cx: &gpui_kit::App,
) -> Vec<String> {
    app.read(cx).palette_titles()
}

/// 命令面板里有一条「导出日志」，按下去状态栏报出落地路径。
///
/// **判据落在状态栏那句话上**，不是"文件真的出现了"：导出目标在
/// 桌面（`dirs::desktop_dir()`），而测试环境不该往用户的桌面写东西。
/// 这条验的是"命令接通了、并且把路径说给用户听了"——那正是用户
/// 按这一下要得到的东西。
#[gpui_kit::test]
fn exporting_the_log_says_where_it_landed(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let (handle, app) = open_app_of_width(cx, 1180.);

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("导出日志", cx);
        settle(window, cx);
    });
    let titles = step_value(cx, handle, |_, cx| palette_titles(&app, cx));
    step(cx, handle, |window, _cx| {
        assert!(
            window.try_find("app:export-log").is_some(),
            "命令面板里该有「导出日志」这一条，实际列出：{titles:?}"
        );
    });

    step(cx, handle, |window, cx| {
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let said = window
            .try_find("status-message")
            .and_then(|element| element.label().map(|label| label.to_string()))
            .unwrap_or_default();
        // 三种可能：导出成功（报路径）、还没有日志（报"没记下任何东西"）、
        // 导出失败（报失败原因）。**三种都算这条命令通了**——
        // 测试环境能不能真写出一个文件不归这条判据管，
        // 归 `logfile::tests` 里的那几条管。
        assert!(
            said.contains("日志已导出到")
                || said.contains("还没有日志可导出")
                || said.contains("导出日志失败"),
            "执行后状态栏该说出结果，实际：{said:?}"
        );
    });
}

#[gpui_kit::test]
fn the_command_palette_speaks_english_all_the_way_through(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let (handle, app) = open_app_of_width(cx, 1180.);

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    let titles = step_value(cx, handle, |_, cx| palette_titles(&app, cx));

    // 这一批把 `describe_pane_command` 那个静态表改成了按需查表，
    // 也把 `build_palette_items` 里二十多处 `format!` 拼的标题改成了查表。
    // **漏一处就是"半翻译"面板**——界面上中英混排，用户分不清哪个是原文
    // 哪个是译文。所以判据是整屏一条中文都不剩，不是抽查某几条。
    //
    // 两条豁免，各有理由（都写在 `i18n.rs` 的表尾注释里）：
    // - `Default Terminal: 终端（Terminal.app）`：终端名是**用户装的那个程序**，
    //   各平台有各自的本地化版本，这里硬编码一份英文只会跟系统对不上；
    //   而 Homebrew / App Store 那些名字本来就该按原样显示（iTerm2 就是 iTerm2）。
    // - `Interface language: 简体中文`：语言名**用该语言自己写**，
    //   跟系统语言选择器一个规矩——英文界面里那条就该写 "简体中文"。
    const EXEMPT: [&str; 2] = ["终端（Terminal.app）", "简体中文"];
    let chinese: Vec<&String> = titles
        .iter()
        .filter(|title| {
            title.chars().any(|c| c as u32 > 0x2E80)
                && !EXEMPT.iter().any(|word| title.contains(word))
        })
        .collect();
    assert!(
        chinese.is_empty(),
        "英文界面里还有 {} 条中文条目：{chinese:?}",
        chinese.len()
    );

    // 也不能是空的：面板该列出命令。空数组会让上面那条断言空过。
    assert!(
        titles.len() >= 20,
        "面板只列出了 {} 条，判据就落空了：{titles:?}",
        titles.len()
    );

    // 抽查三条具体文案，确认查的是**译文**而不是碰巧没查到的原文回落。
    //
    // 挑的是"模板 + 占位符"那几类：它们一个模板要拼出好几条不同的标题，
    // 查表路径不一样（`tr_args` 而不是 `tr`），只查纯文本的那几条验不到它。
    assert!(
        titles.iter().any(|t| t == "Rebind: Move to Trash"),
        "面板里没有「Rebind: Move to Trash」，实际有：{titles:?}"
    );
    assert!(
        titles.iter().any(|t| t == "Go to Downloads"),
        "面板里没有「Go to Downloads」，实际有：{titles:?}"
    );
    assert!(
        titles.iter().any(|t| t == "Transfer Speed: No limit"),
        "面板里没有「Transfer Speed: No limit」，实际有：{titles:?}"
    );
}

#[gpui_kit::test]
fn the_command_palette_stays_chinese_by_default(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let (handle, app) = open_app_of_width(cx, 1180.);

    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
    });
    let titles = step_value(cx, handle, |_, cx| palette_titles(&app, cx));

    assert!(
        titles.len() >= 20,
        "面板只列出了 {} 条，判据落空：{titles:?}",
        titles.len()
    );
    // 同上：删除命令没绑快捷键，`pane_command_enabled` 会把它过滤掉，
    // 所以面板里没有独立的「移到回收站」，只有「改键：移到回收站」。
    assert!(
        titles.iter().any(|t| t == "改键：移到回收站"),
        "中文界面里没有「改键：移到回收站」，实际有：{titles:?}"
    );
    assert!(
        titles.iter().any(|t| t == "前往下载"),
        "中文界面里没有「前往下载」，实际有：{titles:?}"
    );
    // 分组标题也在这一屏里（"文件" / "视图"）——它同样要跟着变。
    assert!(
        !titles.iter().any(|t| t == "Files"),
        "中文界面里出现了英文分组名 Files"
    );
}

#[gpui_kit::test]
fn the_pane_status_bar_speaks_english(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);

    // 中文（默认）。
    let chinese = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "pane-status")
    });
    assert!(
        chinese.contains("个项目"),
        "中文状态栏该说「N 个项目」，实际是 {chinese:?}"
    );

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    // `update_status` 把结果**缓存**进 `self.status`，语言切换不会重算它。
    // 所以这里要点一下列表（触发一次状态变化）才能看到新语言——
    // 这不是缺陷，是"状态栏在事件里算好"这个设计的必然结果，
    // 记在 `update_status` 的文档注释里了。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("down", cx);
    });
    let english = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "pane-status")
    });
    assert!(
        english.contains("item"),
        "英文状态栏该说「N items」，实际是 {english:?}"
    );
    // 一个中文都不剩——漏一处的后果是"半翻译状态栏"。
    assert!(
        !english.chars().any(|c| c as u32 > 0x2E80),
        "英文状态栏里还有中文：{english:?}"
    );
}

#[gpui_kit::test]
fn the_pane_chrome_speaks_english(cx: &mut TestAppContext) {
    // 开在一个刚建的空目录上 → 走"这个文件夹是空的"那条路。
    // （另一条是"没有匹配的项目"，得有查询才走到；那是两句不同的文案。）
    let home = tempfile::tempdir().expect("临时目录");
    let (_session, handle) = open_in(cx, home.path());

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    // 侧栏上那几处（tagline / places title / local）已经在另一条测试里验过，
    // 这条只管窗格里那些：工具条 tooltip 与空态。
    let empty_label = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .try_find("empty-hint-label")
            .and_then(|e| e.label().map(|l| l.to_string()))
            .unwrap_or_default()
    });
    assert!(
        empty_label.contains("empty"),
        "英文空态该说 empty，实际是 {empty_label:?}"
    );
    assert!(
        !empty_label.chars().any(|c| c as u32 > 0x2E80),
        "英文空态里还有中文：{empty_label:?}"
    );

    // 视图名另开一个目录验：空目录走的是 `render_empty` 那条路，
    // 列表本身压根不渲染，`file-list` 这个元素不存在，量不到东西。
    let full = tempfile::tempdir().expect("临时目录");
    std::fs::write(full.path().join("a.txt"), b"x").expect("建文件");
    let (_session2, handle2) = open_in(cx, full.path());
    step(cx, handle2, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });
    let view = step_value(cx, handle2, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "file-list")
    });
    // **断言的是 `List View`（每个词首字母大写），不是 `List view`。**
    // 英文表里这三个 key 曾经各写两条：菜单那份 Title Case、
    // 无障碍名那份小写。因为表按 key 索引成 `HashMap`，
    // **后写的那条静默赢**——所以界面上一直显示的是小写那份，
    // 而菜单里那个字也就跟着成了 "List view"。
    // 现在表里只留 Title Case（读屏念出来大小写没差别，菜单里规整要紧），
    // 所以这里跟着变。改动它会挂的就是这条。
    assert_eq!(view, "List View", "列表视图的无障碍名该跟着语言走");
    // 详情板的多选态（`detail-selection`）**不在这里验**：
    // 多选要 shift 或 Ctrl 点行，而 gpui-kit 的测试 `mouse_down` 把
    // `modifiers` 写死成 `Default::default()`——`window.click` 走的正是
    // 它，造不出带修饰键的那次点击。那两句文案缝成了纯函数
    // （`pane::MultiSelectionSummary`），在 `pane::tests` 里逐语言验。
}

#[gpui_kit::test]
fn the_sidebar_sections_speak_english(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    // 三个分区标题。判据是"一个中文都不剩"，不是"逐个等于某句话"——
    // 后者只能查出你想到的那几条。
    for id in ["sidebar-disks", "sidebar-cloud", "sidebar-remote"] {
        let text = step_value(cx, handle, |window, cx| {
            settle(window, cx);
            a11y_label_at(window, id)
        });
        assert!(!text.is_empty(), "{id} 查不到无障碍名");
        assert!(
            !text.chars().any(|c| c as u32 > 0x2E80),
            "{id} 的英文文案里还有中文：{text:?}"
        );
    }
}

#[gpui_kit::test]
fn an_empty_command_palette_says_so_in_the_chosen_language(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-p", cx);
        settle(window, cx);
    });

    // 先确认默认语言下那句在（不然下面那条挂了我不知道是漏翻还是元素没了）。
    step(cx, handle, |window, cx| {
        window.input("这个词一定搜不到任何命令", cx);
        settle(window, cx);
    });
    let empty = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "palette-empty")
    });
    assert!(
        !empty.is_empty(),
        "搜不到时该有「没有匹配的命令」这一行，实际查不到该元素"
    );
    assert!(
        !empty.chars().any(|c| c as u32 > 0x2E80),
        "英文面板的空态提示里还有中文：{empty:?}"
    );
}

/// 真走一次搬运，日志文件里就该有那一笔。
///
/// **这条是为了堵一个真实的洞**：`logfile` 自己的 9 条测试全是直接调
/// `logfile::log()`，**没有一条走搬运链路**。所以把
/// `ops_entity::log_transfer` 那行删掉，9 条全绿——
/// 而真实运行时后台搬运出了事，日志里一个字都没有。
///
/// 判据落在「搬运完成后去读日志文件」这一层，才能真的守住那行接线。
/// 顺带守住了「日志文件落在临时目录」：读不到文件的话这里就红，
/// 而真机缓存目录被测试写脏这件事**不会有任何测试变红**。
#[gpui_kit::test]
fn a_real_transfer_leaves_a_line_in_the_log(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let src = root.path().join("src");
    many_files(&src);
    std::fs::create_dir(root.path().join("dest")).unwrap();

    let (_session, handle) = open_in(cx, root.path());
    cx.background_executor.allow_parking();

    // 选中 src、复制，进 dest 粘贴 —— 走完整一条搬运链路。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 目录优先，所以 dest(0) 在 src(1) 前。
        window.click(("row-name", 1usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-c", cx);
    });
    step(cx, handle, |window, cx| {
        window.click(("row-name", 0usize), cx);
        window.press("enter", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-v", cx);
    });

    // 等搬运真正落地（日志是任务结束时才写的那一笔）。
    assert!(wait_for(cx, handle, "status-detail"), "搬运该跑起来");
    wait_until_gone(cx, handle, "status-detail");

    let text = step_value(cx, handle, |_, _| {
        crate::logfile::flush();
        let Some(path) = crate::logfile::path() else {
            return String::new();
        };
        std::fs::read_to_string(path).unwrap_or_default()
    });
    assert!(
        text.contains("拷贝 结束"),
        "搬运完成后日志里该有一笔「拷贝 结束…」，实际读到：{text:?}"
    );
    // 日志是排查看用的现场，而密码进日志等于把钥匙串抄给任何
    // 拿到日志文件的人。这一条守着远程连接那条埋点不把 secret 带进去。
    assert!(
        !text.contains("password") && !text.contains("密码"),
        "日志里不该出现凭据字样，实际读到：{text:?}"
    );
}

/// 状态栏那一句在英文界面下也得说英文。
///
/// **这条守的是 `say(cx)` 与 `to_string()` 的差别**：状态栏那句话是
/// `Msg`（结构化错误），而 `Msg` 的 `Display` 恒给中文——它是"还没上屏的
/// 中间态有个能看的字符串"这个约定留下的。写 `.to_string()` 的话，
/// 中文界面下所有测试都绿（`Display` 给的就是中文），
/// 英文界面下整栏不变。一句话里混着两种语言是最难被发现的那种漏翻。
#[gpui_kit::test]
fn the_status_bar_message_speaks_english(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    // 选中 → F2 → 改名 → 回车。做完状态栏上就有「已重命名为「…」」。
    //
    // **改名是插入不是替换**：prompt 预填了原名，而 `input` 从光标处
    // 逐字符往里打，光标在开头，所以 `-second` 插到了 `a.txt` 前面。
    // 这不影响判据——要验的是「这句话在英文界面下变没变」，
    // 不是文件叫什么名字。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
        settle(window, cx);
        window.press("f2", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("-second", cx);
        window.press("enter", cx);
        settle(window, cx);
    });

    let chinese = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "status-message")
    });
    assert!(
        chinese.contains("已重命名为"),
        "中文状态栏该说「已重命名为…」，实际是 {chinese:?}"
    );

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    let english = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "status-message")
    });
    assert_eq!(english, "Renamed to “-seconda.txt”");
    assert!(
        !english.chars().any(|c| c as u32 > 0x2E80),
        "英文状态栏里还有中文：{english:?}"
    );
}

/// 撤销按钮的文案在英文界面下也得说英文。
///
/// 这一句是 `HistoryEntry::describe()` 拼出来的，而它**套在两处**：
/// 按钮标签、tooltip「撤销：{what}（Cmd+Z）」。两处都得跟着语言走。
#[gpui_kit::test]
fn the_undo_button_speaks_english(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    // 一次改名就够：这条验的是「按钮上那句话在英文界面下变没变」，
    // 一个文件名足够说明问题。（两次改名才出现的「X 为 Y」那种长文案
    // 是 `a_long_undo_label_cannot_push_the_status_bar_off_screen`
    // 在管的事——那条量的宽度，不是语言。
    // 实测第二次 F2 之后 prompt 没再打开，所以这里只有一次改名。）
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
        settle(window, cx);
        window.press("f2", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.input("-second", cx);
        window.press("enter", cx);
        settle(window, cx);
    });
    let chinese = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "undo-label")
    });
    assert!(
        chinese.contains("重命名"),
        "中文撤销按钮该说「重命名 X 为 Y」，实际是 {chinese:?}"
    );

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });

    let english = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "undo-label")
    });
    assert_eq!(english, "Renamed “a.txt” to “-seconda.txt”");
    assert!(
        !english.chars().any(|c| c as u32 > 0x2E80),
        "英文撤销按钮上还有中文：{english:?}"
    );
}

/// 确认框的标题与正文在英文界面下也得说英文。
///
/// 正文那句「确定把 N 个项目移到回收站吗？之后还可以撤销。」是**整句带
/// 从句**，不能拆成两条 key——拆了英文那边就是
/// `Move 1 items to the Trash?` 后面跟一句孤立的中文，
/// 夹在英文句子中间的那半句恰恰最显眼。
#[gpui_kit::test]
fn the_trash_confirmation_speaks_english(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| settings.set_locale(crate::i18n::Locale::En));
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
        settle(window, cx);
        // 回收站是破坏性操作，Cmd+Backspace（不是 Delete）。
        window.press("cmd-backspace", cx);
        settle(window, cx);
    });

    let (title, body) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "overlay-title"),
            a11y_label_at(window, "confirm-message"),
        )
    });
    assert_eq!(title, "Move to Trash");
    assert_eq!(body, "Move 1 items to the Trash? This can still be undone.");
    for (what, text) in [("标题", &title), ("正文", &body)] {
        assert!(
            !text.chars().any(|c| c as u32 > 0x2E80),
            "英文确认框的{what}里还有中文：{text:?}"
        );
    }
}

/// 造一个带隐藏项的目录。
///
/// **真名子目录是必须的**：`Listing::read` 里那条「当前目录是隐藏的
/// 就全显示」会把 macOS 上 `tempfile` 给的 `.tmpXXXX` 判成隐藏目录，
/// 于是测试莫名其妙地什么都看得见。详见 `model::tests` 里那条注释。
fn dir_with_hidden_items() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("aaa.txt"), "x").expect("造文件");
    std::fs::write(dir.join(".env"), "x").expect("造文件");
    std::fs::write(dir.join("zzz.txt"), "x").expect("造文件");
    std::fs::create_dir(dir.join(".config")).expect("造目录");
    (root, dir)
}

/// 默认看不见隐藏项，快捷键一下就看见了。
///
/// **判据是列表里那几行真的变了**，不是设置里的布尔值——设置写对了
/// 而 `refilter` 没跑，界面上照样是旧的。
#[gpui_kit::test]
fn the_hidden_switch_reaches_the_list(cx: &mut TestAppContext) {
    let (_root, dir) = dir_with_hidden_items();
    let (_session, handle) = open_in(cx, &dir);

    let before = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(
        before,
        vec!["aaa.txt", "zzz.txt"],
        "默认不该看见 .config / .env"
    );

    step(cx, handle, |window, cx| {
        window.press("cmd-shift-.", cx);
        settle(window, cx);
    });

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(
        after,
        vec![".config", ".env", "aaa.txt", "zzz.txt"],
        "按了 Cmd+Shift+. 之后隐藏项该出现"
    );

    // 切回去：不然只验了"能打开"没验"能关上"。
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-.", cx);
        settle(window, cx);
    });
    let back = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(back, vec!["aaa.txt", "zzz.txt"]);
}

/// 工具栏那颗按钮跟快捷键是**同一个开关**。
///
/// 两条入口各写一遍的话，其中一条改了状态另一条不知道，
/// 用户从按钮切了、界面没变，或者从快捷键切了、按钮不亮。
#[gpui_kit::test]
fn the_hidden_button_and_the_shortcut_are_one_switch(cx: &mut TestAppContext) {
    let (_root, dir) = dir_with_hidden_items();
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("toggle-hidden", cx);
        settle(window, cx);
    });
    let by_button = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert!(
        by_button.contains(&".env".to_string()),
        "点按钮该把隐藏项显示出来，实际 {by_button:?}"
    );

    // 再按快捷键：工具栏按钮那颗要跟着暗回去。
    step(cx, handle, |window, cx| {
        window.press("cmd-shift-.", cx);
        settle(window, cx);
    });
    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (row_names(window), a11y_label_at(window, "toggle-hidden"))
    });
    assert_eq!(after.0, vec!["aaa.txt", "zzz.txt"], "快捷键该把开关关回去");
    // tooltip 是按钮的 label，读不到；改验文案方向：关着的时候
    // 按钮该说"显示隐藏项"，开着的时候该说"隐藏隐藏项"。
    // 这一条走 `aria_label` 挂的那层。
    let tooltip = step_value(cx, handle, |window, _| {
        a11y_label_at(window, "toggle-hidden-label")
    });
    assert!(
        tooltip.contains("显示隐藏项"),
        "关着时按钮该说「显示隐藏项」，实际 {tooltip:?}"
    );
}

/// 开关的状态**存进设置**：换一扇窗打开还是显示着。
///
/// 拆成两半验，因为**没有"关掉应用再重开"的测试接口**（`TestAppContext`
/// 造不出那个动作）。两半合起来覆盖了同一条链：
///
/// 1. 按了快捷键 → 设置里那个布尔值变成 `true`（`toggle_hidden` 末尾
///    那句 `Settings::update` 真的跑了）。
/// 2. 设置里已经是 `true` → 新打开的窗口照着它显示隐藏项
///    （`Listing::read` 真的读了那个设置）。
///
/// 只验第 1 半的话，第 2 半写错了照样绿；反过来也一样。
#[gpui_kit::test]
fn the_hidden_switch_writes_the_setting_and_a_new_window_reads_it(cx: &mut TestAppContext) {
    let (_root, dir) = dir_with_hidden_items();
    let (_session, handle) = open_in(cx, &dir);

    let before = cx.update(|cx| Settings::read(cx).show_hidden);
    assert!(
        !before,
        "隔离出来的设置默认不该开着隐藏项——         前提错了的话下面那些断言都验不到东西"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-shift-.", cx);
        settle(window, cx);
    });

    assert_eq!(
        step_value(cx, handle, |window, cx| {
            settle(window, cx);
            row_names(window)
        }),
        vec![".config", ".env", "aaa.txt", "zzz.txt"],
        "切换之后该看得见隐藏项"
    );
    assert!(
        cx.update(|cx| Settings::read(cx).show_hidden),
        "开关的状态该写进设置里"
    );

    // 第二半：新开一扇窗，照着设置里的 `true` 显示。
    // `open_app` 不传目录，开的是隔离目录 —— 那里没有隐藏项，
    // 所以判据退回设置本身：这一次 `Listing::read` 真的读了它。
    let handle2 = open_app(cx);
    step(cx, handle2, |window, cx| {
        settle(window, cx);
    });
    assert!(
        cx.update(|cx| Settings::read(cx).show_hidden),
        "新窗口不该把这个设置冲掉"
    );
}

/// 进一个隐藏目录，里面的一切都显示出来（用户不用先去开开关）。
#[gpui_kit::test]
fn opening_a_hidden_directory_shows_what_is_inside(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let hidden = root.path().join("outer").join(".config");
    std::fs::create_dir_all(&hidden).expect("建隐藏目录");
    std::fs::write(hidden.join("hosts"), "x").expect("造文件");
    std::fs::write(hidden.join(".gitignore"), "x").expect("造文件");

    let (_session, handle) = open_in(cx, &hidden);

    let names = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(
        names,
        vec![".gitignore", "hosts"],
        "正在看的目录自己是隐藏的，里面就该全显示——\
         否则用户会以为 .config 是空的"
    );
}

/// 详情面板给出**创建时间**，而且是精确到分的那个，不是「3 天前」。
///
/// 判据钉在"精确"上：列表用的 `format::modified` 也会填同一行，
/// 写一个只验"这一行存在"的测试，两种实现都能过。
#[gpui_kit::test]
fn the_details_panel_shows_when_the_file_was_created(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("note.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    let (created, modified) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
        settle(window, cx);
        (
            a11y_label_at(window, "detail-created"),
            a11y_label_at(window, "detail-modified"),
        )
    });

    assert!(
        created.contains('-') && created.contains(':'),
        "创建时间该是精确到分的日期时间，实际 {created:?}"
    );
    assert!(
        modified.contains('-') && modified.contains(':'),
        "详情面板的修改时间也该是精确的（列表才是相对时间），实际 {modified:?}"
    );
    // 新建的文件不该报 1970——那是 epoch 顶上去的假日期。
    assert!(
        !created.starts_with("1970-01-01"),
        "刚建的文件不该显示 1970，实际 {created:?}"
    );
}

/// 目录没有创建时间可言：显示「—」，不拿 mtime 冒充。
#[gpui_kit::test]
fn a_folder_has_no_creation_time_to_show(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::create_dir(dir.path().join("sub")).expect("建目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    // 目录排在文件前面（排序是目录优先），所以第 0 行就是它。
    let created = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
        settle(window, cx);
        a11y_label_at(window, "detail-created")
    });
    assert_eq!(
        created, "—",
        "目录没有创建时间，显示「—」而不是拿修改时间顶上去"
    );
}

/// 一个有目录也有文件的夹子，用来做收藏测试。
///
/// 真名子目录：macOS 上 `tempfile::tempdir()` 给的是 `.tmpXXXX`，
/// 而"正在看的目录自己是隐藏的就全显示"那条规则会让它整个漏进来。
/// 里面的**行序**是 `projects`(0) / `aaa.txt`(1) / `notes.md`(2)——
/// 目录排在文件前面，不是纯按字母序。写行号期望值时按这个来。
fn dir_to_favorite() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::create_dir(dir.join("projects")).expect("建目录");
    std::fs::write(dir.join("notes.md"), "x").expect("造文件");
    std::fs::write(dir.join("aaa.txt"), "x").expect("造文件");
    (root, dir)
}

/// 侧栏「收藏」段列出的那些名字。
fn sidebar_favorites(window: &mut gpui_kit::Window) -> Vec<String> {
    let mut names = Vec::new();
    for index in 0usize..16 {
        let Some(row) = window.try_find(("favorite", index)) else {
            break;
        };
        names.push(row.label().unwrap_or_default().to_string());
    }
    names
}

/// 按 Cmd+D 把选中的那一项收进侧栏，再按一次拿掉。
///
/// 判据是**侧栏里真的多了一行**、再按一次真的少了一行，不是设置里的
/// 数组长度：设置写对了而 `PaneEvent::FavoritesChanged` 没接上，
/// 数组照样变长，界面却纹丝不动。
#[gpui_kit::test]
fn cmd_d_puts_the_selection_in_the_sidebar_and_takes_it_back_out(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    let (_session, handle) = open_in(cx, &dir);

    let before = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        // 一项都没收藏时整段都不该在（空分组比没有更烦人）。
        (
            sidebar_favorites(window),
            a11y_label_at(window, "sidebar-favorites"),
        )
    });
    assert!(
        before.0.is_empty() && before.1.is_empty(),
        "还没收藏任何东西时不该有「收藏夹」这一段，实际 {:?} / {:?}",
        before.0,
        before.1
    );

    // 选中第一行（升序是 projects 目录，它排在最前面）再按 Cmd+D。
    step(cx, handle, |window, cx| {
        window.click(("row-name", 0usize), cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-d", cx);
        settle(window, cx);
    });

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        sidebar_favorites(window)
    });
    assert_eq!(
        after,
        vec!["projects"],
        "按了 Cmd+D 之后侧栏该多出这一行，实际 {after:?}"
    );

    // 再按一次：拿掉。不是"移到最后"——收藏栏里该没有它了。
    step(cx, handle, |window, cx| {
        window.press("cmd-d", cx);
        settle(window, cx);
    });
    let back = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        sidebar_favorites(window)
    });
    assert!(back.is_empty(), "再按一次该取消收藏，实际还剩 {back:?}");
}

/// 收藏**文件**也认：用户 2026-10-14 明确选了"文件也能收"。
#[gpui_kit::test]
fn a_file_can_be_favorited_too(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    let (_session, handle) = open_in(cx, &dir);

    // notes.md 是第 2 行（目录 projects 排最前，然后 aaa.txt、notes.md）。
    step(cx, handle, |window, cx| {
        window.click(("row-name", 2usize), cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-d", cx);
        settle(window, cx);
    });

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        sidebar_favorites(window)
    });
    assert_eq!(
        after,
        vec!["notes.md"],
        "文件也该能收进侧栏，实际 {after:?}"
    );
    // 收藏那一刻它是不是目录，得原样存着——点它才会走"打开"那条路。
    let stored = cx.update(|cx| Settings::read(cx).favorites);
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].is_dir, "收藏的是文件，别把它记成目录");
}

/// 收藏是**新的排在最上面**（用户拍板"加入顺序，新的在最上"）。
#[gpui_kit::test]
fn the_newest_favorite_comes_first(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    let (_session, handle) = open_in(cx, &dir);

    for row in [0usize, 1usize, 2usize] {
        step(cx, handle, |window, cx| {
            window.click(("row-name", row), cx);
            settle(window, cx);
        });
        step(cx, handle, |window, cx| {
            window.press("cmd-d", cx);
            settle(window, cx);
        });
    }

    let after = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        sidebar_favorites(window)
    });
    // 依次收藏第 0、1、2 行 = projects、aaa.txt、notes.md，
    // 所以最后收藏的 notes.md 该排最前面。
    assert_eq!(
        after,
        vec!["notes.md", "aaa.txt", "projects"],
        "最后收藏的那个该排在最前面，实际 {after:?}"
    );
}

/// 没选中东西时按 Cmd+D 要说清楚为什么，而不是默默什么都不做。
#[gpui_kit::test]
fn favoriting_nothing_says_why(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-d", cx);
        settle(window, cx);
    });

    // 读 `pane-status` 而不是 `status-message`：后者是 App 那条
    // "操作结果"消息的 id，而窗格自己写进 `self.status` 的那句
    // 挂在 `pane-status` 上（见 `render_footer`）。
    let said = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "pane-status")
    });
    assert!(
        said.contains("先选中"),
        "该告诉用户先选中一项，实际 {said:?}"
    );
}

/// 后开的窗口认设置里已经有的收藏。
///
/// **为什么不是"收藏栏会当场刷新"**：这个台架测不出来。`settle()` 直接调
/// `window.render_frame()`，那会把窗口根实体整个重画一遍——于是"App
/// 有没有被通知"在这里是隐形的。把 `app::init` 里那句
/// `observe_global::<Settings>` 删掉，上面那 7 条收藏测试照样全绿，
/// 而真实运行时用户按了 Cmd+D 侧栏会纹丝不动。
///
/// 所以判据落在**不依赖重画时机**的那一层：新开一扇窗，它的侧栏是从
/// 设置里读的。这条覆盖了"设置是全局的、跨窗口共享、且真的被读"，
/// 覆盖不了"当帧刷新"——后者靠 `observe_global` 那行（机制已查证：
/// `Settings::update` → `update_global` → `default_global` 推
/// `NotifyGlobalObservers` effect，见 gpui-pre 的 `app.rs`）。
#[gpui_kit::test]
fn a_later_window_reads_the_favorites_already_stored(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        window.click(("row-name", 0usize), cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-d", cx);
        settle(window, cx);
    });

    // 第二扇窗：收藏是全局的，新窗不该从零开始。
    let handle2 = open_app(cx);
    let after = step_value(cx, handle2, |window, cx| {
        settle(window, cx);
        sidebar_favorites(window)
    });
    assert_eq!(
        after,
        vec!["projects"],
        "后开的窗口该也看到这一条收藏，实际 {after:?}"
    );
    assert!(
        cx.read(|cx| Settings::read(cx).is_favorite(&dir.join("projects"))),
        "收藏该落在设置里（新窗口读的就是它）"
    );
}

/// 点收藏项里的**目录**就该进去。
#[gpui_kit::test]
fn clicking_a_favorite_folder_goes_there(cx: &mut TestAppContext) {
    let (_root, dir) = dir_to_favorite();
    std::fs::write(dir.join("projects").join("inner.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        window.click(("row-name", 0usize), cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-d", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        window.click(("favorite", 0usize), cx);
        settle(window, cx);
    });

    let names = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(
        names,
        vec!["inner.txt"],
        "点收藏的目录该进去并看到里面的东西，实际 {names:?}"
    );
}

// ---- 键盘主流程：新建 / 重命名 / 删除 / 撤销 / 粘贴 / 批量改名 ----
//
// 这一批补的是**真实键盘路径上的落盘结果**。补之前的情况：
//
//   * `f2` 只在两条 i18n 测试里顺带走过，断言的是按钮/状态栏**文案**，
//     没有一条断言"文件真的改了名"；
//   * `cmd-backspace` 只验过确认框的英文，删完到底删没删、能不能撤销，
//     没有任何断言；
//   * **`cmd-z` 从来没有被按过**——而「可撤销」是这个项目写在侧栏上的
//     第一条卖点。`ops.rs` 里那批 `undo_*` 单测覆盖的是纯逻辑层，
//     "键 → handler → 确认框 → 落盘"这条接线断了它们照样全绿。
//
// 每条都同时断言**磁盘**与**界面**：列表刷新了但文件没改名（或反过来）
// 是两条独立的链路，只验一条会漏掉另一条。

/// 目录里此刻的文件名，排好序——用来断言"磁盘上真的变了"。
fn names_on_disk(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("读目录")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 等一个磁盘上的条件成立。
///
/// 回收站与搬运都在后台任务里跑，而 `settle()` 只重画界面、不推进任务。
/// 每轮得先 `run_until_parked` 让后台结果回流，再画帧让订阅者消费——
/// 跟 [`wait_for`] 是同一套道理，只是判据落在磁盘上而不是某个元素上。
fn wait_for_disk(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    mut probe: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        let done = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                probe()
            })
            .expect("窗口还在");
        if done || Instant::now() > deadline {
            return done;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 等到**应用状态栏**（`status-message`）满足 `ready` 为止，返回最终那句话。
///
/// **等的是"这一步真的收尾了"，不是磁盘上的中间态。** 两者差着一段时间：
/// 文件已经从原目录消失（磁盘探测已经成功），但 `ops_entity::finish_job`
/// 还没跑，撤销栈还是空的。这时候按 `cmd-z` 会得到「没有可撤销的操作」，
/// 于是「该弹确认框」那步就成了薛定谔的。状态栏是 `finish_job` 写的，
/// 等它才是等对了。
///
/// 注意别和 [`wait_for_status`] 混了：那个读的是**搜索面板**的
/// `search-status`，还顺带推虚拟时钟（搜索有 180 ms 防抖）。这里的删除
/// 任务跑在真正的 OS 线程上，推时钟没用，得给它真实时间。
fn wait_for_app_status(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    ready: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        let text = cx
            .update_window(handle.into(), |_, window, cx| {
                settle(window, cx);
                a11y_label_at(window, "status-message")
            })
            .expect("窗口还在");
        if ready(&text) || Instant::now() > deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `cmd-shift-n` 新建文件夹：走完输入框，目录里真的多一个。
///
/// 顺带守住"预填名不冲突"：`ops::fresh_folder_path` 预填
/// 「未命名文件夹」，所以真实用户按一下回车就能建出来。
#[gpui_kit::test]
fn cmd_shift_n_creates_a_folder_on_disk(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-shift-n", cx);
    });

    let asked = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("prompt-ok").is_some()
    });
    assert!(asked, "Cmd+Shift+N 该弹出新建文件夹的输入框");

    // 预填名已经是不冲突的，直接确认——这就是"按一下回车"那条路。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("prompt-ok", cx);
    });

    let on_disk = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        names_on_disk(&dir)
    });
    assert_eq!(
        on_disk,
        vec!["未命名文件夹"],
        "确认之后磁盘上该多出一个文件夹"
    );

    let listed = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(listed, vec!["未命名文件夹"], "新建的文件夹该出现在列表里");
}

/// `cmd-alt-n` 新建空白文件：预填「未命名.txt」，确认后磁盘上真的多一个**空文件**。
///
/// 顺带钉住两件事：预填名按模板后缀来（不是「未命名文件夹」），
/// 空白模板的内容是 0 字节（不是写了个换行）。
#[gpui_kit::test]
fn cmd_alt_n_creates_an_empty_file_on_disk(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-alt-n", cx);
    });

    let asked = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window.try_find("prompt-ok").is_some()
    });
    assert!(asked, "Cmd+Alt+N 该弹出新建文件的输入框");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("prompt-ok", cx);
    });

    let (on_disk, contents) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            names_on_disk(&dir),
            std::fs::read_to_string(dir.join("未命名.txt")).unwrap_or_default(),
        )
    });
    assert_eq!(on_disk, vec!["未命名.txt"], "确认之后磁盘上该多出一个文件");
    assert_eq!(contents, "", "空白模板就是 0 字节");
}

/// `f2` 改名：输入新名字、回车，磁盘上真的换了名。
///
/// 输入框预填的是原名、光标落在开头，所以先 `cmd-a` 全选再打字
/// ——跟真实用户"F2 之后直接敲新名字"是同一个效果。
#[gpui_kit::test]
fn f2_renames_the_selected_file_on_disk(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("old.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("f2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("renamed.txt", cx);
        window.press("enter", cx);
        settle(window, cx);
    });

    let on_disk = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        names_on_disk(&dir)
    });
    assert_eq!(
        on_disk,
        vec!["renamed.txt"],
        "F2 改名该真的落到磁盘上，旧名不该还在"
    );

    let listed = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        row_names(window)
    });
    assert_eq!(listed, vec!["renamed.txt"], "列表该跟着显示新名字");
}

/// 按 Esc 关掉改名框：名字一个字都不该变。
///
/// 边界场景——"打开对话框"与"关掉对话框"是两条路，只验前者的话
/// 关闭路径断了（比如 Esc 没接上 dismiss）不会有测试变红。
#[gpui_kit::test]
fn escape_leaves_the_name_untouched(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("keep.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("f2", cx);
    });
    // 先把对话框改得面目全非，再按 Esc —— 这样"没生效"才有说服力。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("should-not-stick.txt", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("escape", cx);
    });

    let (closed, on_disk) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (window.try_find("prompt-ok").is_none(), names_on_disk(&dir))
    });
    assert!(closed, "Esc 该把改名框关掉");
    assert_eq!(on_disk, vec!["keep.txt"], "取消之后磁盘上不该有任何变化");
}

/// 改名撞上已有的名字：对话框留着，磁盘一个字不动。
///
/// 异常场景。判据是"被拒"而不是"报了什么错"——错误那句挂在
/// `render_prompt` 里一个没有 element id 的 div 上，读不到 a11y 文案，
/// 所以退回两个**能观测**的事实：框还开着、旧文件还在。
#[gpui_kit::test]
fn renaming_onto_a_taken_name_changes_nothing(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("a.txt"), "a").expect("造文件");
    std::fs::write(dir.join("b.txt"), "b").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    // 升序第一行是 a.txt，把它改成已经存在的 b.txt。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("f2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("b.txt", cx);
        window.press("enter", cx);
        settle(window, cx);
    });

    let (still_asking, on_disk) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (window.try_find("prompt-ok").is_some(), names_on_disk(&dir))
    });
    assert!(
        still_asking,
        "名字被占了该把对话框留着让人改，而不是静默关掉"
    );
    assert_eq!(on_disk, vec!["a.txt", "b.txt"], "被拒时不该动磁盘上的东西");
}

/// 删除到回收站，再用 `cmd-z` 把它拿回来。
///
/// **这是这个项目第一卖点的端到端验证**：`cmd-backspace` → 确认 →
/// 文件进回收站 → `cmd-z` → 确认 → 文件回到原处。
/// 纯逻辑层的 `undo_move_puts_everything_back` 验的是 `ops::` 里的恢复
/// 算法，验不到"用户按 Cmd+Z 到底走不走到它"。
///
/// **这条曾经必挂，原因是两处，都查清了**（别再写成"缺权限"糊过去）：
/// 1. 删除本身没坏——`trash` crate 走 AppleScript 驱动 Finder，文件确实
///    进了回收站。一度以为是"没给终端自动化 Finder 权限（`-10004`）"，
///    实测不是那个拦路虎。
/// 2. 真正坏的是**撤销**：那时用 `fs::read_dir` 枚举 `~/.Trash` 找同名项，
///    而 macOS 的 TCC **不给普通进程读回收站的目录项**
///    （`Operation not permitted`，shell / launchd 用户代理 / `.app`
///    三种上下文实测一致）。现在改成按候选名逐个 `stat`
///    （见 `ops::restore_from_trash_dir`）——被拒的只有"枚举"这一步，
///    按已知路径 `stat` / `rename` 是放行的，所以**不需要「完全磁盘访问
///    权限」也能放回**。
///
/// 计时上也不能偷懒：**别等磁盘上的中间态**。文件从原目录消失时
/// `ops_entity::finish_job` 还没跑，撤销栈还是空的，那时候按 `cmd-z`
/// 会得到「没有可撤销的操作」。要等状态栏——它是 `finish_job` 写的。
///
/// 往别的机器 / CI 上搬之前先确认 `trash::delete` 这条路可用
/// （探针：建个临时文件调一次，看返回值）。
#[gpui_kit::test]
fn cmd_backspace_trashes_the_selection_and_undo_puts_it_back(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("doomed.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);
    // 删除跑在后台，回调线程要能唤醒任务。
    cx.background_executor.allow_parking();

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-backspace", cx);
    });

    // 破坏性操作：先问一句，不能默默就删。
    let asked = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        a11y_label_at(window, "confirm-message")
    });
    assert!(
        asked.contains("回收站"),
        "移到回收站前该先确认，实际确认框写的是 {asked:?}"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("confirm-ok", cx);
    });

    // 等 `finish_job`：它写状态栏，也在这时把撤销记录压栈。
    let status = wait_for_app_status(cx, handle, |text| {
        text.contains("已移到回收站") || text.contains("没有项目能移进回收站")
    });
    assert!(
        !status.contains("没有项目能移进回收站"),
        "确认之后文件该进回收站，状态栏却写着 {status:?}。\n\
         先怀疑这台机器上 `trash::delete` 的 Finder 路径不通：\n\
         建个临时文件直接调一次，看返回的 Err。"
    );
    assert!(
        !dir.join("doomed.txt").exists(),
        "状态栏说进了回收站，原目录里却还在：{status:?}"
    );

    // ---- 撤销 ----
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-z", cx);
    });
    let (undo_asks, message) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            window.try_find("confirm-ok").is_some(),
            a11y_label_at(window, "confirm-message"),
        )
    });
    assert!(
        undo_asks,
        "撤销前也该确认一句。没弹说明撤销栈是空的——那就是「等磁盘中间态」\n\
         又冒出来了，得等 `finish_job` 写的状态栏。实际消息 {message:?}"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("confirm-ok", cx);
    });

    let undone = wait_for_app_status(cx, handle, |text| {
        text.contains("已撤销") || text.contains("撤销失败")
    });
    assert!(
        !undone.contains("撤销失败"),
        "撤销该成功，状态栏却写着 {undone:?}"
    );
    assert!(
        dir.join("doomed.txt").exists(),
        "撤销该把文件放回原处——「之后还可以撤销」是确认框自己给出的承诺"
    );
}

/// 没东西可撤销时按 `cmd-z`：说清楚，而不是弹一个空确认框。
#[gpui_kit::test]
fn undo_with_nothing_to_undo_says_so(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-z", cx);
    });

    let (said, asked) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "status-message"),
            window.try_find("confirm-ok").is_some(),
        )
    });
    assert!(
        said.contains("没有可撤销"),
        "空栈时该告诉用户没得撤，实际状态栏写的是 {said:?}"
    );
    assert!(!asked, "没东西可撤销时不该弹确认框");
}

/// 复制到另一栏粘贴：文件真的落进目标目录。
///
/// 这条走的是 `cmd-c` / `cmd-v` 而不是 F5——`f5_copies_...` 已经覆盖了
/// F5，但"复制 + 切栏 + 粘贴"是另一条键路，中间多了一次焦点切换，
/// 而焦点没切过去的话粘贴会落在原目录（静默地复制成"X 2"）。
#[gpui_kit::test]
fn copying_in_one_pane_and_pasting_in_the_other_lands_the_file(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    std::fs::write(left.join("report.txt"), "hi").unwrap();

    cx.background_executor.allow_parking();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    // 焦点栏选中左栏那一项，复制。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(slot_label(window, 0), "焦点栏");
        window
            .within(("pane-slot", 0usize))
            .click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-c", cx);
    });

    // 焦点挪到另一栏——这一步漏了的话，粘贴会落在左栏自己身上。
    step(cx, handle, |window, cx| {
        window.press("tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(slot_label(window, 1), "焦点栏", "焦点该挪到右栏了");
        window.press("cmd-v", cx);
    });

    let landed = wait_for_disk(cx, handle, || right.join("report.txt").exists());
    assert!(landed, "粘贴之后目标目录该多出这个文件");
    assert!(
        left.join("report.txt").exists(),
        "复制（不是剪切）不该动源文件"
    );
    // 界面也该跟着刷新出这一行。
    let landed_in_ui = wait_for_row(cx, handle, 1, "report.txt");
    assert!(landed_in_ui, "右栏列表该刷新出粘进来的文件");
}

/// 批量重命名：改一次模板，选中的每个文件都跟着改。
///
/// 此前 `cmd-shift-r` 只在三条**布局**测试里出现，都是量对话框宽度
/// （`every_dialog_stays_inside_a_narrow_window` 一类），预览算得对不对、
/// 点"执行"有没有落盘，没有任何断言。
#[gpui_kit::test]
fn batch_renaming_renames_the_whole_selection(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.join(name), "x").expect("造文件");
    }
    let (_session, handle) = open_in(cx, &dir);
    cx.background_executor.allow_parking();

    // 全选 → 打开批量重命名。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-shift-r", cx);
    });

    // 模板默认是 `{name}{ext}`（什么都不改），换成加个后缀。
    // 打开时焦点就在模板框上（`create_bulk_inputs` 末尾那次 focus），
    // 先全选再打字。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("{name}-2026{ext}", cx);
        settle(window, cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("bulk-ok", cx);
    });

    let expected = vec!["a-2026.txt".to_string(), "b-2026.txt".to_string()];
    let done = wait_for_disk(cx, handle, || names_on_disk(&dir) == expected);
    assert!(
        done,
        "两个文件都该按模板改名，实际磁盘上是 {:?}",
        names_on_disk(&dir)
    );
}

// ---- 粘贴冲突 ----
//
// 「冲突四策略」是这个项目的横切主题，而 UI 层此前**一条都没有**：
// 纯逻辑层的 `resolve_conflict` 有单测，"对话框弹没弹出来、按下去走的
// 是不是那个策略"没人守——把 `render_conflict` 从浮层列表里摘掉，
// 粘贴会直接覆盖目标，而所有测试照样绿。

/// 两栏各有一份同名文件（`left/a.txt` 与 `right/a.txt` 内容不同）。
fn two_panes_with_a_clash() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("临时目录");
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).expect("建目录");
    std::fs::create_dir(&right).expect("建目录");
    // 内容故意不同：这样"覆盖了 / 没覆盖"一眼分得开。
    std::fs::write(left.join("a.txt"), "from-left").expect("造文件");
    std::fs::write(right.join("a.txt"), "from-right").expect("造文件");
    (root, left, right)
}

/// 开成双栏（焦点在左），把左栏那一项复制、挪到右栏粘贴。
fn copy_from_left_into_right(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>) {
    cx.background_executor.allow_parking();
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .within(("pane-slot", 0usize))
            .click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        window.press("cmd-c", cx);
    });
    step(cx, handle, |window, cx| {
        window.press("tab", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert_eq!(slot_label(window, 1), "焦点栏", "焦点该挪到右栏了");
        window.press("cmd-v", cx);
    });
}

/// 撞上同名文件：先把选择权交回给用户，三条路都摆出来。
#[gpui_kit::test]
fn pasting_over_an_existing_name_asks_first(cx: &mut TestAppContext) {
    let (_root, left, right) = two_panes_with_a_clash();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    copy_from_left_into_right(cx, handle);

    let asked = wait_for(cx, handle, "conflict-skip");
    assert!(asked, "目标里有同名文件时该先问，而不是默默覆盖或默默跳过");

    let (has_replace, has_keep_both) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            window.try_find("conflict-replace").is_some(),
            window.try_find("conflict-keep-both").is_some(),
        )
    });
    assert!(has_replace, "该有「替换」这一条路");
    assert!(has_keep_both, "该有「保留两者」这一条路");
}

/// 选「跳过」：目标一个字节都不动。
#[gpui_kit::test]
fn choosing_skip_leaves_the_target_untouched(cx: &mut TestAppContext) {
    let (_root, left, right) = two_panes_with_a_clash();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    copy_from_left_into_right(cx, handle);
    assert!(wait_for(cx, handle, "conflict-skip"), "该先问怎么处理");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("conflict-skip", cx);
    });
    wait_until_gone(cx, handle, "overlay-card");

    let content = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        std::fs::read_to_string(right.join("a.txt")).unwrap_or_default()
    });
    assert_eq!(
        content, "from-right",
        "跳过就该什么都不动，实际目标文件是 {content:?}"
    );
}

/// 选「替换」：目标被覆盖成源的那份。
#[gpui_kit::test]
fn choosing_replace_overwrites_the_target(cx: &mut TestAppContext) {
    let (_root, left, right) = two_panes_with_a_clash();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    copy_from_left_into_right(cx, handle);
    assert!(wait_for(cx, handle, "conflict-replace"), "该先问怎么处理");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("conflict-replace", cx);
    });

    let replaced = wait_for_disk(cx, handle, || {
        std::fs::read_to_string(right.join("a.txt"))
            .map(|text| text == "from-left")
            .unwrap_or(false)
    });
    assert!(
        replaced,
        "替换之后目标该是源那份，实际 {:?}",
        std::fs::read_to_string(right.join("a.txt"))
    );
    assert!(
        left.join("a.txt").exists(),
        "替换不该动源文件（这是粘贴不是移动）"
    );
}

/// 选「保留两者」：多出一个带序号的兄弟，原来的那份不动。
#[gpui_kit::test]
fn choosing_keep_both_adds_a_numbered_sibling(cx: &mut TestAppContext) {
    let (_root, left, right) = two_panes_with_a_clash();
    let (_session, handle) = open_session(
        cx,
        &Session {
            tabs: vec![left.clone(), right.clone()],
            active: 0,
            split: Some(1),
            focus_on_left: true,
            sync: false,
        },
    );

    copy_from_left_into_right(cx, handle);
    assert!(wait_for(cx, handle, "conflict-keep-both"), "该先问怎么处理");

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("conflict-keep-both", cx);
    });

    let both = wait_for_disk(cx, handle, || names_on_disk(&right).len() == 2);
    assert!(
        both,
        "保留两者之后该有两份，实际 {:?}",
        names_on_disk(&right)
    );
    // 原来那份的内容不该被动过。
    assert_eq!(
        std::fs::read_to_string(right.join("a.txt")).unwrap_or_default(),
        "from-right",
        "保留两者不该动原来的那份"
    );
}

/// 破坏性操作必须**先确认**：确认框弹出来的那一刻，磁盘上一个字都还没动。
///
/// 这一条刻意不碰"真删成功"——[`cmd_backspace_trashes_the_selection_and_undo_puts_it_back`]
/// 那条要走系统回收站，而 macOS 上的 `trash` crate 是靠 AppleScript 驱动
/// **Finder** 干的（实测错误：`“Finder”遇到一个错误：发生权限违例 (-10004)`）。
/// 没给终端授予"自动化 Finder / 辅助功能"的机器上它必然失败，
/// 那是环境前提不是产品缺陷，所以能在这台机器上验证的那一半单独放这里。
#[gpui_kit::test]
fn trashing_asks_before_touching_the_disk(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("doomed.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-backspace", cx);
    });

    // 确认框出来了，但文件还好好地在。
    let (asked, on_disk) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (window.try_find("confirm-ok").is_some(), names_on_disk(&dir))
    });
    assert!(asked, "移到回收站前该先弹确认框");
    assert_eq!(on_disk, vec!["doomed.txt"], "确认之前不该动磁盘");

    // 取消：也是什么都不动。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("confirm-cancel", cx);
    });
    let (closed, after) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (window.try_find("confirm-ok").is_none(), names_on_disk(&dir))
    });
    assert!(closed, "点取消该把确认框关掉");
    assert_eq!(after, vec!["doomed.txt"], "取消之后文件该还在");
}

/// 可移动介质上按同一个删除键，必须变成**彻底删除**，而且要在确认框里
/// 说清两件事：为什么不能进回收站、这一步不能撤销。
///
/// 判据盯的是**文案**：用户按的是「删除」，程序不能悄悄换成不可撤销的操作
/// 还不吭声。策略接没接上、文案有没有说清，这条都会红。
#[gpui_kit::test]
fn a_removable_volume_asks_before_deleting_for_good(cx: &mut TestAppContext) {
    let _lock = crate::volumes::test_guard();
    crate::volumes::set_removable_override(Some(true));

    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
        settle(window, cx);
        window.press("cmd-backspace", cx);
        settle(window, cx);
    });

    let (title, body) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "overlay-title"),
            a11y_label_at(window, "confirm-message"),
        )
    });
    crate::volumes::set_removable_override(None);

    assert_eq!(title, "彻底删除", "介质上不该再提回收站");
    assert!(
        body.contains("可移动介质"),
        "正文要说明为什么不能进回收站：{body}"
    );
    assert!(body.contains("不能撤销"), "必须说清不可撤销：{body}");
    assert!(
        !body.contains("之后还可以撤销"),
        "别给用户留能撤销的错觉：{body}"
    );
}

/// 固定磁盘上显式要求彻底删除（`Cmd+Shift+Backspace`）：不进回收站、
/// 也不能撤销。这条不依赖介质判定，走的是用户明确的那条路。
#[gpui_kit::test]
fn the_permanent_delete_shortcut_skips_the_trash(cx: &mut TestAppContext) {
    let _lock = crate::volumes::test_guard();
    crate::volumes::set_removable_override(Some(false));

    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, dir.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-a", cx);
        settle(window, cx);
        window.press("cmd-shift-backspace", cx);
        settle(window, cx);
    });

    let (title, body) = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        (
            a11y_label_at(window, "overlay-title"),
            a11y_label_at(window, "confirm-message"),
        )
    });
    crate::volumes::set_removable_override(None);

    assert_eq!(title, "彻底删除");
    assert!(body.contains("不会进回收站"), "要说清后果：{body}");
    assert!(body.contains("不能撤销"), "要说清不可撤销：{body}");
}

// ---- 右键菜单 ----
//
// 这批钉的是"右键那一按落在哪儿，菜单就对着谁"：行上对着这一批、空白处
// 对着当前目录、祖先列对着那一条路径、侧栏对着那一条。右键只是第二扇门
// ——真动手的还是原来那批命令（`PaneCommand` / `WFilesApp` 的方法）——
// 所以断言尽量落在**结果**上：文件真改了名、目录里真多出个文件夹、
// 标签真多了一栏、设置里真少了一条连接。
//
// 菜单实体是**下一帧**才建的（`ContextMenu` 用 `window.defer`），
// 所以每条都先 `settle` 一次再找菜单（`settle` 自己推两帧）。

/// 菜单里此刻有哪些项（按显示顺序）。分隔线不占名字。
fn menu_labels(window: &mut gpui_kit::Window) -> Vec<String> {
    (0..32)
        .map(|index| menu_item_label(window, index))
        .filter(|label| !label.is_empty())
        .collect()
}

/// 点菜单里叫这个名字的那一项。
fn click_menu_item(window: &mut gpui_kit::Window, label: &str, cx: &mut gpui_kit::App) {
    let index = (0..32)
        .find(|&index| menu_item_label(window, index) == label)
        .unwrap_or_else(|| panic!("菜单里该有「{label}」，实际是：{:?}", menu_labels(window)));
    window.click(index, cx);
}

/// 右击一行文件：菜单对着**这一行**，改名改的也是它。
#[gpui_kit::test]
fn the_row_menu_acts_on_the_row_under_the_pointer(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("a.txt"), "x").expect("造文件");
    std::fs::write(dir.join("b.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 右击**第二行**：改的该是 b.txt，不是第一行。
        window.right_click(("row-name", 1usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"重命名".to_string()),
            "行上的菜单该有「重命名」：{labels:?}"
        );
        assert!(
            !labels.contains(&"新建文件夹".to_string()),
            "按在行上不该弹出目录菜单：{labels:?}"
        );
        click_menu_item(window, "重命名", cx);
    });

    // 改名框预填的是那一行的名字，全选之后直接敲新的。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("prompt-ok").is_some(),
            "菜单里点「重命名」该打开改名框"
        );
        window.press("cmd-a", cx);
    });
    step(cx, handle, |window, cx| {
        window.input("renamed.txt", cx);
        window.press("enter", cx);
        settle(window, cx);
    });

    let on_disk = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        names_on_disk(&dir)
    });
    assert_eq!(
        on_disk,
        vec!["a.txt", "renamed.txt"],
        "改的该是右键点中的那一个（b.txt），a.txt 一个字母都不该动"
    );
}

/// 右击一条**已经在选中集合里**的行：菜单对着整批，选中项不塌。
///
/// 这条顺手钉住一个事实：右击派发的是 `on_aux_click` 而**不是** `on_click`
/// ——后者才是"点一下就把多选塌成一个"的那条路。接错了的话，右击完
/// 再点菜单里任何批量操作，作用范围就悄悄缩水成一个文件。
#[gpui_kit::test]
fn right_clicking_a_selected_row_keeps_the_batch(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), "x").expect("造文件");
    }
    let (_session, handle) = open_in(cx, &dir);

    // 选中前两行：点第一行，再用 shift+下 扩到第二行。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("shift-down", cx);
    });

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.right_click(("row-name", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        click_menu_item(window, "移到回收站", cx);
    });

    // 确认框上写着"几个项目"：塌成一个的话这里就只剩 1。
    let message = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        window
            .try_find("confirm-message")
            .and_then(|message| message.label().map(|label| label.to_string()))
            .unwrap_or_default()
    });
    assert!(
        message.contains("2 个项目"),
        "右击已选中的行不该把整批塌成一个，确认框该说 2 个：{message}"
    );

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.click("confirm-cancel", cx);
    });
    let on_disk = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        names_on_disk(&dir)
    });
    assert_eq!(on_disk.len(), 3, "取消了就一个都不该动：{on_disk:?}");
}

/// 右击列表空白处：菜单对着**当前目录**，新建的文件夹真落在这儿。
#[gpui_kit::test]
fn the_blank_menu_makes_a_folder_here(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        // 列表正中间：只有一行文件，剩下全是空白。
        window.right_click("file-list", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"新建文件夹".to_string()),
            "空白处该有「新建文件夹」：{labels:?}"
        );
        assert!(
            labels.contains(&"粘贴".to_string()),
            "空白处该有「粘贴」：{labels:?}"
        );
        assert!(
            !labels.contains(&"重命名".to_string()),
            "空白处不该弹出文件菜单：{labels:?}"
        );
        click_menu_item(window, "新建文件夹", cx);
    });

    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(
            window.try_find("prompt-ok").is_some(),
            "「新建文件夹」该弹出输入框"
        );
        // 预填名已经是不冲突的，直接确认——按一下回车那条路。
        window.click("prompt-ok", cx);
    });

    let on_disk = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        names_on_disk(&dir)
    });
    assert!(
        on_disk.contains(&"未命名文件夹".to_string()),
        "新建的文件夹该落在当前目录里：{on_disk:?}"
    );
}

/// 网格里的格子也挂着同一份文件菜单。
#[gpui_kit::test]
fn a_grid_tile_carries_the_file_menu(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::write(dir.join("a.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir);

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-2", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.right_click(("tile", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"重命名".to_string()),
            "格子上的菜单该有「重命名」：{labels:?}"
        );
        assert!(
            labels.contains(&"快速查看".to_string()),
            "格子上的菜单该有「快速查看」：{labels:?}"
        );
    });
}

/// 分列视图里**祖先列**的一条：它不在当前目录的选中集合里，
/// 菜单只做认得路径的那几件（复制路径 / 用系统打开），不冒充文件菜单。
#[gpui_kit::test]
fn an_earlier_column_carries_the_path_menu(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("plain");
    std::fs::create_dir(&dir).expect("建目录");
    std::fs::create_dir(dir.join("one")).expect("建目录");
    std::fs::write(dir.join("one").join("inner.txt"), "x").expect("造文件");
    let (_session, handle) = open_in(cx, &dir.join("one"));

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.press("cmd-3", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let mut last = 0usize;
        while window.try_find(("column", last + 1)).is_some() {
            last += 1;
        }
        assert!(last >= 1, "分列视图该有祖先列");
        window
            .within(("column", last - 1))
            .right_click(("column-row", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"用系统打开".to_string()),
            "祖先列的菜单该有「用系统打开」：{labels:?}"
        );
        assert!(
            !labels.contains(&"重命名".to_string()),
            "祖先列的行不在选中集合里，不该冒充文件菜单：{labels:?}"
        );
    });
}

/// 侧栏一条"位置"的右键菜单：在新标签中打开真的多一栏。
#[gpui_kit::test]
fn the_sidebar_menu_opens_a_place_in_a_new_tab(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("临时目录");
    let (_session, handle) = open_in(cx, root.path());

    step(cx, handle, |window, cx| {
        settle(window, cx);
        window.right_click(("place", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"在新标签中打开".to_string()),
            "侧栏菜单该有「在新标签中打开」：{labels:?}"
        );
        assert!(
            labels.contains(&"在系统文件管理器里打开".to_string()),
            "侧栏菜单该有「在系统文件管理器里打开」：{labels:?}"
        );
        click_menu_item(window, "在新标签中打开", cx);
    });

    let tabs = step_value(cx, handle, |window, cx| {
        settle(window, cx);
        tab_count(window)
    });
    assert_eq!(tabs, 2, "点「在新标签中打开」该真多一栏");
}

/// 远程那一条：删连接之前先问一句，问完真的从设置里消失。
#[gpui_kit::test]
fn the_remote_menu_asks_before_forgetting_the_connection(cx: &mut TestAppContext) {
    let _session = isolate_session();
    let handle = open_app(cx);
    step(cx, handle, |_, cx| {
        Settings::update(cx, |settings| {
            settings.remotes.push(crate::remote::Config::new(
                "jg".to_string(),
                "坚果云".to_string(),
                crate::remote::Kind::WebDav,
                // 端口 9 是 discard 端口：连不上，也不会真有请求出去。
                "http://127.0.0.1:9/dav/".to_string(),
                "me".to_string(),
            ));
        });
    });

    step(cx, handle, |window, cx| {
        // 远程那一段在折叠线以下：先滚到它，再右键。
        scroll_sidebar(window, cx);
        window.right_click(("remote", 0usize), cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let labels = menu_labels(window);
        assert!(
            labels.contains(&"删除连接".to_string()),
            "远程那一行该有「删除连接」：{labels:?}"
        );
        click_menu_item(window, "删除连接", cx);
    });

    // 先问：弹的是标准确认框，里面写着删的是哪一条。
    step(cx, handle, |window, cx| {
        settle(window, cx);
        let asked = window
            .try_find("confirm-message")
            .and_then(|message| message.label().map(|label| label.to_string()))
            .unwrap_or_default();
        assert!(
            asked.contains("坚果云"),
            "确认框该写明删的是哪条连接：{asked}"
        );
        window.click("confirm-ok", cx);
    });

    let left = cx.update(|cx| Settings::peek(cx, |settings| settings.remotes.len()));
    assert_eq!(left, 0, "确认之后那条连接该从设置里消失");
}
