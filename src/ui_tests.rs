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
use crate::model::{Sort, SortKey};
use crate::session::Session;
use crate::settings::Settings;
use crate::terminal;

/// 开一个装着 WFilesApp 的测试窗口。
fn open_app(cx: &mut TestAppContext) -> gpui_kit::WindowHandle<Root> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::app::init(cx);
        crate::pane::init(cx);

        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1180.), px(760.)),
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
    let session = isolate_session();
    Session {
        tabs: vec![dir.to_path_buf()],
        active: 0,
    }
    .save_to(&session.path().join("session.json"))
    .expect("写测试会话");
    let handle = open_app(cx);
    (session, handle)
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
        saved.view_for(dir.path()),
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
    );
    seeded
        .save_to(&session.path().join("settings.json"))
        .expect("写测试设置");
    Session {
        tabs: vec![dir.path().to_path_buf()],
        active: 0,
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
