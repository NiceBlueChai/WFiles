//! 无头 UI 集成测试：真的开一个窗口、真的派发按键，
//! 断言命令面板从打开到执行完整跑通，而不只是“代码看起来对”。
//!
//! 放在 `#[cfg(test)]` 模块里而不是 `tests/` 目录：本项目是二进制 crate，
//! 集成测试目录链接不到 `WFilesApp`。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions};
use gpui_kit::{base::Root, px, size};
use gpui_kit::point;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::app::WFilesApp;
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
    cx.background_executor.allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::app::init(cx);
        crate::pane::init(cx);

        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(760.)),
        };
        let (window, app) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| WFilesApp::new(window, cx)),
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

/// 会话与设置隔到临时目录，免得测试动到用户真实的标签恢复状态。
///
/// `WFILES_SESSION_DIR` 是**进程级**的环境变量，而 `cargo test` 默认多线程跑：
/// 两个测试同时改它，后写的那个会把前一个的目录顶掉，于是断言去读一个
/// 从没写过的路径。所以这里配一把锁，谁拿着锁谁才独占这个变量，
/// 拿到 `SessionIsolation` 的那一刻起一直到测试结束都是它的。
fn isolate_session() -> SessionIsolation {
    static LOCK: Mutex<()> = Mutex::new(());
    // 上一个持锁的线程 panic 过就把锁抢回来，别把整轮测试卡死。
    let guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("临时目录");
    unsafe { std::env::set_var("WFILES_SESSION_DIR", dir.path()) };
    SessionIsolation { _guard: guard, dir }
}

/// 见 [`isolate_session`]：持锁期间独占 `WFILES_SESSION_DIR`。
struct SessionIsolation {
    _guard: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl SessionIsolation {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// 画两帧：滚动容器里的内容要第二帧才完成布局并注册进观察表，
/// 单帧之后 `try_find` 会漏掉刚出现的子元素。
fn settle(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    window.render_frame(cx);
    window.render_frame(cx);
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

/// 某一栏标的是"焦点栏"还是"另一栏"。
fn slot_label(window: &mut gpui_kit::Window, slot: usize) -> String {
    window
        .try_find(("pane-slot", slot))
        .map(|row| row.label().unwrap_or_default().to_string())
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

    let saved = Settings::load_from(&session.path().join("settings.json"))
        .expect("设置应该已经落盘");
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
        assert_eq!(row_names(window), ["a.txt", "b.txt", "c.txt"], "默认是按名字");
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
        std::fs::write(dir.join(format!("f{ix}.bin")), vec![ix as u8; FILE_SIZE as usize]).unwrap();
    }
    FILE_COUNT as u64 * FILE_SIZE
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0)
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
        let done = cx.update(|cx| ops.read(cx).running().map_or(0, |running| running.bytes_done));
        if done >= at_least || Instant::now() > deadline {
            return done;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 此刻的已拷贝字节数（顺手把已就绪的消息推一次）。
fn bytes_now(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>) -> u64 {
    cx.run_until_parked();
    cx.update(|cx| ops.read(cx).running().map_or(0, |running| running.bytes_done))
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

fn message_of(cx: &mut TestAppContext, ops: &gpui_kit::Entity<Ops>) -> String {
    cx.update(|cx| {
        ops.read(cx)
            .last_message()
            .map(str::to_string)
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
        cx.update(|cx| ops.read(cx).retry_label()),
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
fn wait_for_row(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>, slot: usize, name: &str) -> bool {
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

    assert_eq!(file_count(&root.path().join("dest").join("src")), FILE_COUNT);
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
        assert_eq!(
            row_names_in(window, 1),
            vec!["only.txt"],
            "右栏该跟着进去"
        );
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
            window.try_find("header").is_none()
                && window.try_find(("header", 0usize)).is_some(),
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
        assert!(last.y > first.y, "第 12 项该换行到下面去：{first:?} {last:?}");
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
        assert_eq!(column_rows(window, parent), vec!["one"], "临时目录里只有 one");
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
        assert_eq!(
            view_of(window),
            "网格",
            "打开这个目录时该套用记住的视图"
        );
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

/// 造一个装了两个文本文件的 `bundle.zip`，放在 `dir` 里。
fn make_bundle(dir: &Path) -> std::path::PathBuf {
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.join(name), "hello").unwrap();
    }
    let zip = dir.join("bundle.zip");
    crate::archive::create(
        &zip,
        dir,
        &[dir.join("a.txt"), dir.join("b.txt")],
    )
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
    step(cx, handle, |window, cx| {
        window.click("op-archive", cx);
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
        assert!(
            window.try_find("key-capture").is_none(),
            "录完就该收起浮层"
        );
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
        assert!(window.try_find("term-close").is_some(), "面板上要有关闭按钮");
        window.click("term-close", cx);
    });
    step(cx, handle, |window, cx| {
        settle(window, cx);
        assert!(window.try_find("terminal").is_none(), "关闭按钮应该收起面板");
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
        assert!(row_names(window).contains(&"inner.rs".to_string()), "该在它所在的目录里");
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
    assert!(listed.contains("note.txt"), "标成红色的该能被搜到：{listed}");

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
    assert!(changed.contains("改过的第二行"), "该显示右边的新内容：{changed}");

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
        if found.as_ref().is_some_and(|label| !label.is_empty())
            || Instant::now() > deadline
        {
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
        assert!(
            window.try_find("connect-ok").is_some(),
            "该弹出连接对话框"
        );
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
        seen.iter().any(|it| it.starts_with("PUT /dav/folder/a.txt")),
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
        window
            .try_find("detail-云盘")
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
    assert!(again < Duration::from_millis(800), "撤销限速该立刻生效：{again:?}");
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
