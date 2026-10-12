//! 内嵌终端实体：起一个 PTY、收输出、把格子摆到界面上、收键盘。
//!
//! 屏幕与按键编码的纯逻辑在 [`crate::term`]，这一层只做四件脏活：
//! 起进程、跨线程搬字节、量格子尺寸、把按键换成字节写回去。
//!
//! 两条边界值得记着：
//! - 子进程跑在真正的 OS 线程上，字节靠通道送回来，UI 线程全程不阻塞；
//! - 关掉面板一定会杀掉子进程（见 [`Drop`]），不留一个看不见的 shell。

use std::{
    cell::Cell,
    io::{Read as _, Write as _},
    path::PathBuf,
    rc::Rc,
};

use gpui_kit::component::{
    ActiveTheme, Sizable as _,
    button::{Button, ButtonVariants as _},
    *,
};
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, Rgba,
    ScrollDelta, ScrollWheelEvent, Task, Window, canvas, div, px, rgb, *,
};

use crate::term;

/// 开关面板的键。终端聚焦时也要能按到它，所以绑定与"放行"两处共用这一个常量。
pub const TOGGLE_KEY: &str = "ctrl-alt-t";

/// 面板高度（像素）。再矮就只剩几行，跑个 `ls` 都看不全。
const HEIGHT: f32 = 240.;
/// 格子区四周留白。
const PADDING: f32 = 8.;
/// 字号。
const FONT_SIZE: f32 = 13.;
/// 一行比字高多出来的那点空隙，纯粹为了不挤。
const LEADING: f32 = 2.;

/// 再大的窗口也不给超过这个行列数：一个格子一个字符串，
/// 几万格会把一帧拖成几百毫秒。
const MAX_ROWS: u16 = 120;
const MAX_COLS: u16 = 300;

/// 终端自己的底色与前景。
///
/// 刻意不跟主题走：终端是"一块屏幕"，深色底能让 `ls --color`、git 这些
/// 按 ANSI 配色输出的东西保持本来面目。浅色主题下也是同一块屏——
/// 没人会指望终端跟着主题变白。
const SURFACE: u32 = 0x181818;
const INK: u32 = 0xd4d4d4;
/// 选区底色，半透明：压在字下面但不盖住字。
const SELECTION: u32 = 0x3a6ea57f;

/// xterm 的标准十六色。
const ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, //
    0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, //
    0x3b8eea, 0xd670d6, 0x29b8db, 0xf5f5f5, //
];

/// 面板要告诉应用的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// 用户按了关闭。
    Close,
}

/// 一个格子多大。字体不变，量一次就够。
#[derive(Clone, Copy, Debug)]
struct Metrics {
    width: Pixels,
    height: Pixels,
}

/// 一个正在跑的 PTY。
///
/// `master` 留着是为了改尺寸（PTY 的尺寸得告诉内核，子进程才会收到
/// `SIGWINCH`）；`writer` 是喂键盘的那头；`killer` 单独留一份，
/// 因为等子进程退出的那个线程把 `child` 本身拿走了。
struct Process {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

pub struct Terminal {
    screen: term::Screen,
    process: Option<Process>,
    /// 探针量出来的格子区：窗口坐标 + 像素尺寸。
    area: Rc<Cell<Bounds<Pixels>>>,
    metrics: Option<Metrics>,
    /// 起在哪儿。
    dir: PathBuf,
    /// 起的是什么，标题栏要显示。
    program: String,
    focus: FocusHandle,
    selection: Option<term::Selection>,
    dragging: bool,
    /// 子进程已经结束了：退出码或信号名。
    exited: Option<String>,
    /// 起不来时给个能看懂的原因，而不是"按了没反应"。
    error: Option<String>,
    output: Option<Task<()>>,
    watching: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Focusable for Terminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Terminal {
    pub fn new(dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            screen: term::Screen::new(24, 80),
            process: None,
            area: Rc::new(Cell::new(Bounds::default())),
            metrics: None,
            dir,
            program: term::shell(),
            focus: cx.focus_handle(),
            selection: None,
            dragging: false,
            exited: None,
            error: None,
            output: None,
            watching: None,
        };
        this.start(cx);
        this
    }

    /// 起一个 shell。失败时把原因留在面板里，而不是静默。
    fn start(&mut self, cx: &mut Context<Self>) {
        let (rows, cols) = self.screen.size();
        let program = self.program.clone();
        let dir = self.dir.clone();

        let opened = (|| -> Result<Opened, String> {
            let system = portable_pty::native_pty_system();
            let pair = system
                .openpty(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| format!("打不开终端：{error}"))?;

            let mut command = portable_pty::CommandBuilder::new(&program);
            command.cwd(&dir);
            // 不设 TERM，很多程序（git、ls、vim）只会吐纯文本。
            command.env("TERM", "xterm-256color");
            let child = pair
                .slave
                .spawn_command(command)
                .map_err(|error| format!("起不来 {program}：{error}"))?;
            let killer = child.clone_killer();

            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|error| format!("读不到终端输出：{error}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|error| format!("写不进终端：{error}"))?;
            Ok(Opened {
                master: pair.master,
                writer,
                child,
                killer,
                reader,
            })
        })();

        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.error = Some(error);
                self.exited = None;
                cx.notify();
                return;
            }
        };

        let Opened {
            master,
            writer,
            mut child,
            killer,
            mut reader,
        } = opened;
        self.process = Some(Process {
            master,
            writer,
            killer,
        });
        self.error = None;
        self.exited = None;
        self.selection = None;

        // 读输出：真正的 OS 线程，读完一段就往通道里丢一段。
        let (out_tx, out_rx) = async_channel::unbounded::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    // 子进程关掉了终端，读端就是 EOF，这一趟到此为止。
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if out_tx.try_send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // 等退出：wait 会一直阻塞，所以也得是独立线程。
        let (exit_tx, exit_rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let status = child.wait().ok();
            exit_tx.try_send(status).ok();
        });

        let output = cx.spawn(async move |this, cx| {
            let mut cx = cx.clone();
            while let Ok(bytes) = out_rx.recv().await {
                this.update(&mut cx, |this, cx| {
                    this.screen.feed(&bytes);
                    cx.notify();
                })
                .ok();
            }
        });
        let watching = cx.spawn(async move |this, cx| {
            let mut cx = cx.clone();
            if let Ok(status) = exit_rx.recv().await {
                let message = match status {
                    None => "已结束".to_string(),
                    Some(status) => match status.signal() {
                        Some(signal) => format!("被 {signal} 结束"),
                        None => format!("已结束（退出码 {}）", status.exit_code()),
                    },
                };
                this.update(&mut cx, |this, cx| {
                    this.exited = Some(message);
                    cx.notify();
                })
                .ok();
            }
        });
        self.output = Some(output);
        self.watching = Some(watching);
        cx.notify();
    }

    /// 杀掉子进程。
    ///
    /// 不能只是把 `process` 丢掉：PTY 的读端被读线程拿着，
    /// 光丢这一头 shell 还活着——它会一直占着一个终端，而用户根本看不见它。
    fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.killer.kill().ok();
        }
    }

    /// 杀掉重开。历史一起清掉：留着一个已经死掉的会话的历史更让人困惑。
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.stop();
        let (rows, cols) = self.screen.size();
        self.screen = term::Screen::new(rows, cols);
        self.start(cx);
    }

    /// 量一个格子多大。字体不变，量一次存着。
    fn measure(&mut self, cx: &Context<Self>) {
        if self.metrics.is_some() {
            return;
        }
        let system = cx.text_system();
        let font = gpui_kit::font(cx.theme().mono_font_family.clone());
        let font_id = system.resolve_font(&font);
        let size = px(FONT_SIZE);
        // 'M' 是等宽字体里最宽的那个，拿它当格子宽不会挤。
        let width = system
            .advance(font_id, size, 'M')
            .map(|advance| advance.width)
            .unwrap_or(px(FONT_SIZE * 0.6));
        let height = system.ascent(font_id, size) + system.descent(font_id, size) + px(LEADING);
        self.metrics = Some(Metrics { width, height });
    }

    /// 面板尺寸变了就跟着改行数与列数，并告诉 PTY。
    fn fit(&mut self, cx: &mut Context<Self>) {
        let Some(metrics) = self.metrics else {
            return;
        };
        let area = self.area.get();
        let cols = clamp(
            (area.size.width / metrics.width).floor(),
            term::MIN_COLS,
            MAX_COLS,
        );
        let rows = clamp(
            (area.size.height / metrics.height).floor(),
            term::MIN_ROWS,
            MAX_ROWS,
        );
        if self.screen.size() == (rows, cols) {
            return;
        }
        self.screen.resize(rows, cols);
        // 尺寸一变，选中区对应的就不是原来那些格子了。
        self.selection = None;
        if let Some(process) = self.process.as_mut() {
            process
                .master
                .resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .ok();
        }
        cx.notify();
    }

    /// 窗口坐标落在哪一格。
    fn cell_at(&self, position: Point<Pixels>) -> Option<term::Cell> {
        let metrics = self.metrics?;
        let area = self.area.get();
        let (rows, cols) = self.screen.size();
        let col = ((position.x - area.origin.x) / metrics.width).floor();
        let row = ((position.y - area.origin.y) / metrics.height).floor();
        if row < 0.0 || col < 0.0 || row >= rows as f32 || col >= cols as f32 {
            return None;
        }
        Some(term::Cell {
            row: row as u16,
            col: col as u16,
        })
    }

    /// 往终端里写一段字节。
    fn write(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        // 一敲键盘就回到最新：这是所有终端的默契。
        self.screen.scroll_home();
        self.selection = None;
        match self.process.as_mut() {
            Some(process) => {
                if process
                    .writer
                    .write_all(bytes)
                    .and_then(|_| process.writer.flush())
                    .is_err()
                {
                    self.error = Some("写不进终端".to_string());
                }
            }
            None => self.error = Some("终端没在跑".to_string()),
        }
        cx.notify();
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.filter(|it| !it.is_empty()) else {
            return;
        };
        let text = self.screen.selected_text(selection);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// 把当前这一整屏复制走。
    fn copy_visible(&mut self, cx: &mut Context<Self>) {
        let text = self.screen.plain();
        if !text.trim().is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        // 程序开了括号粘贴就包起来：否则多行文本会被当成一条条命令立刻执行。
        let bracketed = self.screen.bracketed_paste();
        let bytes = term::paste(&text, bracketed);
        self.write(&bytes, cx);
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // 面板自己的开关必须放行给应用，否则终端一聚焦就再也按不掉它。
        if keystroke.unparse() == TOGGLE_KEY {
            return;
        }
        // 剩下的全部收下：Tab 补全不该去换窗格，Cmd+P 也不该在这时弹命令面板。
        cx.stop_propagation();

        // Cmd 是宿主的：复制与粘贴。
        if keystroke.modifiers.platform {
            match (keystroke.key.as_str(), keystroke.modifiers.shift) {
                // 拖不出选区的时候（比如输出已经滚过去了），整屏复制是唯一的出路。
                ("c", true) => self.copy_visible(cx),
                ("c", false) => self.copy_selection(cx),
                ("v", _) => self.paste(cx),
                _ => {}
            }
            return;
        }

        // 翻回滚用 Shift+PageUp/PageDown：不带 Shift 的那一下要留给程序
        // （less、vim 都靠它翻页）。
        let (rows, _) = self.screen.size();
        if keystroke.modifiers.shift && !keystroke.modifiers.control && !keystroke.modifiers.alt {
            match keystroke.key.as_str() {
                "pageup" => {
                    self.screen.scroll_up(rows as usize);
                    cx.notify();
                    return;
                }
                "pagedown" => {
                    self.screen.scroll_down(rows as usize);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        if let Some(bytes) = term::encode_key(keystroke, self.screen.application_cursor()) {
            self.write(&bytes, cx);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.dragging = true;
        // 先记成"点到哪儿就是哪儿"，拖动时再拉出范围；只点一下的话
        // 它就是个空选区，等于取消选中。
        self.selection = self
            .cell_at(event.position)
            .map(|cell| term::Selection::new(cell, cell));
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        // 鼠标在别处松开的话，移动事件里就带不到"按着左键"这件事——
        // 那就当这次拖动结束了，不然选区会一直跟着鼠标跑。
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = false;
            cx.notify();
            return;
        }
        if let (Some(selection), Some(cell)) = (self.selection, self.cell_at(event.position)) {
            self.selection = Some(term::Selection {
                anchor: selection.anchor,
                head: cell,
            });
            cx.notify();
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.dragging = false;
        // 拖出来的是空选区就是"点了一下"，不算选中。
        self.selection = self.selection.filter(|selection| !selection.is_empty());
        cx.notify();
    }

    fn wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(lines) => lines.y,
            // 触控板给的是像素，按行高折算成行。
            ScrollDelta::Pixels(pixels) => match self.metrics {
                Some(metrics) => pixels.y / metrics.height,
                None => pixels.y / px(FONT_SIZE + LEADING),
            },
        };
        let lines = lines.round();
        if lines >= 1.0 {
            self.screen.scroll_down(lines as usize);
        } else if lines <= -1.0 {
            // 往下滚是看更新的，往上滚才是翻历史。
            self.screen.scroll_up(-lines as usize);
        } else {
            return;
        }
        cx.notify();
    }

    /// 标题栏那一行：跑的是什么，以及现在什么状态。
    fn subtitle(&self) -> String {
        // 标题里只显示程序名：`/bin/zsh` 这种完整路径太占地方，也没信息量。
        let program = std::path::Path::new(&self.program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.program.clone());
        let mut text = match (&self.error, &self.exited) {
            (Some(error), _) => format!("{program} · {error}"),
            (_, Some(exited)) => format!("{program} · {exited}"),
            _ => format!("{program} · {}", self.dir.display()),
        };
        // 翻着历史时说一声，不然用户会以为输出卡住了。
        if self.screen.offset() > 0 {
            text.push_str(&format!(" · 已上翻 {} 行", self.screen.offset()));
        }
        // 全屏程序（vim、less）用自己的缓冲区，回滚里什么都没有——
        // 与其让人一直往上翻翻出空行，不如直说。
        if self.screen.alternate_screen() {
            text.push_str(" · 全屏程序");
        }
        text
    }

    // ---- 渲染 ----

    fn render_header(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let subtitle = self.subtitle();
        h_flex()
            .h(px(32.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .id("term-subtitle")
                    .test_support()
                    .aria_label(subtitle.clone())
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(subtitle),
            )
            .child(div().flex_1())
            .child(
                Button::new("term-restart")
                    .label("重启")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
            )
            .child(
                Button::new("term-close")
                    .label("关闭")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|_this, _, _, cx| cx.emit(TerminalEvent::Close))),
            )
            .into_any_element()
    }

    fn render_row(&self, row: u16, metrics: Metrics) -> gpui_kit::AnyElement {
        let mut element = h_flex()
            .h(metrics.height)
            .w_full()
            .items_center()
            .whitespace_nowrap();
        for run in self.screen.line(row) {
            if run.text.is_empty() {
                continue;
            }
            // 反显就是把前后景对调，界面不需要知道"反显"这回事。
            let (fg, bg) = if run.face.inverse {
                (run.face.bg, run.face.fg)
            } else {
                (run.face.fg, run.face.bg)
            };
            let mut span = div().text_color(paint(fg, INK));
            if !matches!(bg, term::Color::Default) {
                span = span.bg(paint(bg, SURFACE));
            }
            if run.face.bold {
                span = span.font_bold();
            }
            if run.face.italic {
                span = span.italic();
            }
            element = element.child(span.child(run.text));
        }
        element.into_any_element()
    }

    fn render_selection(&self, metrics: Metrics) -> gpui_kit::AnyElement {
        let Some(selection) = self.selection.filter(|it| !it.is_empty()) else {
            return div().into_any_element();
        };
        let (from, to) = selection.bounds();
        let width = (to.col - from.col + 1) as f32 * metrics.width.as_f32();
        let height = (to.row - from.row + 1) as f32 * metrics.height.as_f32();
        div()
            .id("term-selection")
            .test_support()
            .absolute()
            .top(px(from.row as f32 * metrics.height.as_f32()))
            .left(px(from.col as f32 * metrics.width.as_f32()))
            .w(px(width))
            .h(px(height))
            .bg(gpui_kit::rgba(SELECTION))
            .into_any_element()
    }

    /// 光标画成格子底下的一条横杠：不用反色块盖住那个字符，
    /// 也就不用为了一个光标把整行拆成三段。
    fn render_cursor(&self, metrics: Metrics) -> gpui_kit::AnyElement {
        let Some((row, col)) = self.screen.cursor() else {
            return div().into_any_element();
        };
        let top = (row as f32 + 1.) * metrics.height.as_f32() - 2.;
        div()
            .absolute()
            .top(px(top))
            .left(px(col as f32 * metrics.width.as_f32()))
            .w(px(metrics.width.as_f32()))
            .h(px(2.))
            .bg(rgb(INK))
            .into_any_element()
    }

    /// 量出格子区有多大。
    ///
    /// GPUI 没有元素级的 `on_resize`，也没有"读父容器尺寸"的接口，
    /// 所以插一个绝对定位、铺满、什么都不画的 canvas：
    /// 它布局完拿到自己的 bounds（正好是格子区），写进共享单元格，
    /// 下一帧 [`Terminal::fit`] 读出来算行列。
    fn render_probe(&self) -> gpui_kit::AnyElement {
        let area = self.area.clone();
        canvas(
            move |bounds: Bounds<Pixels>, _, _| area.set(bounds),
            |_, _, _, _| {},
        )
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .into_any_element()
    }
}

impl Render for Terminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        self.measure(cx);
        self.fit(cx);

        let metrics = self.metrics.unwrap_or(Metrics {
            width: px(FONT_SIZE * 0.6),
            height: px(FONT_SIZE + LEADING),
        });
        let (rows, _) = self.screen.size();
        let mono = cx.theme().mono_font_family.clone();
        let border = cx.theme().border;

        // 整屏文本挂成 aria_label：读屏软件念的是"这一屏写了什么"，
        // 而不是几百个各自一个词的格子；集成测试也靠它断言终端里出现了什么。
        let mut grid = div()
            .id("term-grid")
            .test_support()
            .aria_label(self.screen.plain())
            .relative()
            .size_full()
            .child(self.render_selection(metrics));

        for row in 0..rows {
            grid = grid.child(self.render_row(row, metrics));
        }
        grid = grid
            .child(self.render_cursor(metrics))
            .child(self.render_probe());

        v_flex()
            .id("terminal")
            .test_support()
            .h(px(HEIGHT))
            .flex_shrink_0()
            .border_t_1()
            .border_color(border)
            .bg(rgb(SURFACE))
            .text_color(rgb(INK))
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(PADDING))
                    .font_family(mono)
                    .text_size(px(FONT_SIZE))
                    .child(
                        grid.track_focus(&self.focus)
                            .key_context("Terminal")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event, window, cx| {
                                    this.mouse_down(event, window, cx)
                                }),
                            )
                            .on_mouse_move(cx.listener(|this, event, window, cx| {
                                this.mouse_move(event, window, cx)
                            }))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, event, window, cx| {
                                    this.mouse_up(event, window, cx)
                                }),
                            )
                            .on_scroll_wheel(
                                cx.listener(|this, event, window, cx| {
                                    this.wheel(event, window, cx)
                                }),
                            )
                            // 捕获阶段就收下按键：默认的全局绑定（Tab 换窗格、
                            // Esc 清选中）不该趁机执行。
                            .capture_key_down(cx.listener(|this, event, window, cx| {
                                this.key_down(event, window, cx)
                            })),
                    ),
            )
    }
}

/// 一次成功的 spawn 拿回来的几样东西。
struct Opened {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    reader: Box<dyn std::io::Read + Send>,
}

/// 把行列数夹到合理范围：窗口缩到 0 不能真的要 0 行，拉得再大也不能要几万格。
fn clamp(value: f32, min: u16, max: u16) -> u16 {
    let value = if value.is_finite() { value } else { 0. };
    (value as i64).clamp(min as i64, max as i64) as u16
}

/// 一个格子的颜色。`Default` 用终端自己的底/前景，不跟主题走。
fn paint(color: term::Color, fallback: u32) -> Rgba {
    match color {
        term::Color::Default => rgb(fallback),
        term::Color::Indexed(index) => ansi(index),
        term::Color::Rgb(r, g, b) => rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
    }
}

/// 256 色：前十六个是标准色，中间是 6×6×6 的立方，最后是灰阶。
fn ansi(index: u8) -> Rgba {
    match index {
        0..=15 => rgb(ANSI[index as usize]),
        16..=231 => {
            let offset = index - 16;
            let r = offset / 36;
            let g = offset / 6 % 6;
            let b = offset % 6;
            rgb((cube(r) << 16) | (cube(g) << 8) | cube(b))
        }
        _ => {
            let level = 8 + (index as u32 - 232) * 10;
            rgb((level << 16) | (level << 8) | level)
        }
    }
}

fn cube(level: u8) -> u32 {
    if level == 0 {
        0
    } else {
        55 + level as u32 * 40
    }
}
