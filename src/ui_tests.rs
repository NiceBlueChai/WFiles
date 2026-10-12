//! 无头 UI 集成测试：真的开一个窗口、真的派发按键，
//! 断言命令面板从打开到执行完整跑通，而不只是“代码看起来对”。
//!
//! 放在 `#[cfg(test)]` 模块里而不是 `tests/` 目录：本项目是二进制 crate，
//! 集成测试目录链接不到 `WFilesApp`。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions};
use gpui_kit::{base::Root, px, size};
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
/// 窗格一打开就开始监听目录，而 notify 的回调线程随时可能把事件送回来
/// 唤醒任务——测试调度器把"别的线程来唤醒"判成不确定行为并直接报错。
/// 这不是被测代码的问题：监听本来就跨线程。所以开了窗口就统一放开。
fn open_app(cx: &mut TestAppContext) -> gpui_kit::WindowHandle<Root> {
    let handle = open_app_of_width(cx, 1180.);
    cx.background_executor.allow_parking();
    handle
}

/// 指定窗口宽度开一个：网格的列数取决于窗格有多宽，要测就得能改它。
fn open_app_of_width(cx: &mut TestAppContext, width: f32) -> gpui_kit::WindowHandle<Root> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::app::init(cx);
        crate::pane::init(cx);

        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(760.)),
        };
        let (window, _view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| WFilesApp::new(window, cx)),
        )
        .expect("打开测试窗口");
        window.downcast::<Root>().expect("Base Root")
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
        cx.new(|_| Ops::new())
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
    let handle = open_app_of_width(cx, width);

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
