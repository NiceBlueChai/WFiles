//! 搜索面板实体：一个输入框、一份结果、一趟后台搜索。
//!
//! 判定与遍历的活都在 [`crate::search`]，这里只管三件脏活：
//! 把输入变成查询、把后台线程送回来的结果摆成列表、把 Enter 变成"跳到那个文件"。
//!
//! 两条边界值得记着：
//! - 每敲一个字都会重开一趟，但旧的那趟一定先被中止（丢掉 [`search::Cancel`] 就是中止），
//!   不会有两个线程同时往一份结果里塞东西；
//! - 结果靠"第几趟"（`generation`）认主人，晚到的旧批次一律丢掉。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, Sizable as _, VirtualListScrollHandle,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    scroll::{ScrollableElement as _, ScrollbarAxis},
    v_virtual_list, *,
};
use gpui_kit::{
    App, Context, EventEmitter, FocusHandle, Focusable as _, KeyDownEvent, Render, ScrollStrategy,
    Size, Subscription, Task, Window, div, px, size, transparent_white, *,
};

use crate::format;
use crate::pane::PaneCommand;
use crate::query::Query;
use crate::search::{self, Hit, Search, Stats};

/// 面板里每一行的高度：名字一行、底下一行放路径或命中行。
const ROW_HEIGHT: f32 = 44.;
/// 敲完字等多久再真的开搜。每敲一个字都扫一遍目录太浪费，
/// 而这个长度已经短到不影响手感。
const DEBOUNCE: Duration = Duration::from_millis(180);
const CARD_WIDTH: f32 = 720.;
const CARD_MAX_HEIGHT: f32 = 560.;
/// 面板里最多列多少条。几千条的列表翻不完，
/// 而它们得先全进内存才能显示——不如早停在两千。
const PANEL_LIMIT: usize = 2_000;

/// 面板告诉窗格的两件事。
#[derive(Clone, Debug)]
pub enum SearchEvent {
    /// 关掉面板（Esc、点遮罩、点关闭按钮）。
    Close,
    /// 关掉面板并跳到这个文件：先切到它所在的目录，再选中它。
    Reveal(PathBuf),
}

pub struct SearchPanel {
    /// 从哪个目录往下搜。
    root: PathBuf,
    input: Entity<InputState>,
    /// 解析好的查询；语法有错或者还是空的时候是 `None`。
    query: Option<Query>,
    /// 语法错误，红字显示在输入框下面。
    error: Option<String>,
    hits: Vec<Hit>,
    cursor: usize,
    /// 上一趟走完的账，用来写"找到几个、扫了多少项"。
    stats: Option<Stats>,
    running: bool,
    /// 上一趟的中止把手；换成新的那一趟时旧的被丢掉，搜索随之停下。
    cancel: Option<search::Cancel>,
    /// 去抖与收结果这两个任务；丢掉即放弃。
    task: Option<Task<()>>,
    /// 只看根目录这一层（不进子目录）。
    shallow: bool,
    /// 正在给这一趟搜索起名字（存下来的那一行显示出来了）。
    naming: bool,
    /// 下一帧要把焦点给名字输入框。见 [`Self::render`]。
    focus_name: bool,
    name_input: Entity<InputState>,
    /// 输入框的订阅得一直攥在手里：`Subscription` 一丢，订阅就断了。
    /// 这个字段因此只写不读——它就是为了让订阅活到面板销毁那一刻。
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
    /// 第几趟。过期的批次靠它认出来。
    generation: usize,
    scroll: VirtualListScrollHandle,
    row_sizes: Rc<Vec<Size<Pixels>>>,
}

impl SearchPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("找文件：name: ext: type: size: date: content:")
        });
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("给这个搜索起个名字"));
        let name_subscription = cx.subscribe_in(
            &name_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_current(window, cx);
                }
            },
        );
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.schedule(cx);
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));

        Self {
            root,
            input,
            query: None,
            error: None,
            hits: Vec::new(),
            cursor: 0,
            stats: None,
            running: false,
            cancel: None,
            task: None,
            shallow: false,
            naming: false,
            focus_name: false,
            name_input,
            subscriptions: vec![subscription, name_subscription],
            generation: 0,
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
        }
    }

    /// 把输入框换成这句查询并立刻开搜。存下来的搜索点一下走这里。
    pub fn set_query(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.shallow = false;
        self.schedule(cx);
    }

    /// 给当前这趟搜索起个名字存下来。
    fn save_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().trim().to_string();
        let query = self.input.read(cx).value().trim().to_string();
        if name.is_empty() || query.is_empty() {
            return;
        }
        crate::settings::Settings::update(cx, |settings| {
            settings.save_search(name, query, self.root.clone());
        });
        self.naming = false;
        self.name_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// 忘掉一条存下来的搜索。
    fn forget(&mut self, name: String, cx: &mut Context<Self>) {
        crate::settings::Settings::update(cx, |settings| settings.forget_search(&name));
        cx.notify();
    }

    /// 输入框的焦点柄。
    ///
    /// 面板刚建好时输入框还没挂到窗口上，那时候抢焦点抢不到，
    /// 所以由窗格在面板挂上去之后拿着这个柄去 focus。
    pub fn input_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// 输入变了：等一小会儿再开搜。
    ///
    /// 连着敲字时只会跑最后一趟——每次调度都把趟号加一，
    /// 到点的那个回调发现号不对就什么都不做。
    fn schedule(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let token = self.generation;
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let mut cx = cx.clone();
            this.update(&mut cx, |this, cx| this.restart(token, cx))
                .ok();
        }));
    }

    /// 真的开一趟：解析、起后台线程、准备收结果。
    fn restart(&mut self, token: usize, cx: &mut Context<Self>) {
        if token != self.generation {
            return;
        }
        // 先停旧的：丢掉把手即中止，旧线程自己会退出。
        self.cancel = None;
        self.task = None;

        let text = self.input.read(cx).value().to_string();
        let query = match Query::parse(&text) {
            Ok(query) => query,
            Err(error) => {
                self.query = None;
                self.error = Some(error);
                self.hits.clear();
                self.stats = None;
                self.running = false;
                self.sync_rows();
                cx.notify();
                return;
            }
        };
        self.error = None;

        if query.is_empty() {
            self.query = None;
            self.hits.clear();
            self.cursor = 0;
            self.stats = None;
            self.running = false;
            self.sync_rows();
            cx.notify();
            return;
        }

        self.query = Some(query.clone());
        self.hits.clear();
        self.cursor = 0;
        self.stats = None;
        self.running = true;
        self.sync_rows();
        cx.notify();

        let generation = self.generation;
        let (tx, rx) = async_channel::unbounded::<search::Event>();
        let mut search = Search::new(self.root.clone(), query)
            .with_limit(PANEL_LIMIT)
            // 打过的标签与标过的颜色也要能搜（`tag:` / `color:`）。
            .with_marks(crate::settings::Settings::read(cx).marks.clone());
        if self.shallow {
            search = search.with_depth(0);
        }
        self.cancel = Some(search::spawn(
            search,
            Arc::new(move |event| {
                tx.try_send(event).ok();
            }),
        ));

        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        // 号的趟已经作废：这一批是上一趟的，丢掉。
                        if this.generation != generation {
                            return false;
                        }
                        match event {
                            search::Event::Batch(mut batch) => {
                                this.hits.append(&mut batch);
                                this.sync_rows();
                            }
                            search::Event::Done(stats) => {
                                this.running = false;
                                this.stats = Some(stats);
                                this.cancel = None;
                                this.sync_rows();
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn sync_rows(&mut self) {
        if self.row_sizes.len() != self.hits.len() {
            self.row_sizes = Rc::new(vec![size(px(0.), px(ROW_HEIGHT)); self.hits.len()]);
        }
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.hits.is_empty() {
            return;
        }
        let cursor = self.cursor as i64 + delta as i64;
        self.cursor = cursor.clamp(0, self.hits.len() as i64 - 1) as usize;
        self.scroll
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// 打开光标所在的那个文件。
    fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(self.cursor) else {
            return;
        };
        cx.emit(SearchEvent::Reveal(hit.path.clone()));
    }

    fn reveal_at(&mut self, index: usize, cx: &mut Context<Self>) {
        self.cursor = index;
        self.reveal_cursor(cx);
    }

    /// 结果区上面那一行字：正在搜 / 找到几个 / 出错 / 还没输东西。
    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.query.is_none() {
            return "输入要找的东西。content: 会连文件内容一起找。".to_string();
        }
        let mut text = if self.running {
            format!("正在搜… 已找到 {}", self.hits.len())
        } else {
            format!("找到 {} 个", self.hits.len())
        };
        if let Some(stats) = self.stats {
            text.push_str(&format!(" · 扫了 {} 项", stats.visited));
            if stats.truncated {
                text.push_str(" · 到上限了，把条件再收紧一点");
            }
        }
        text
    }

    /// 相对于搜索根目录的路径：结果几十条时，靠它分得清是哪个文件。
    fn location(&self, hit: &Hit) -> String {
        let path = hit.path.strip_prefix(&self.root).unwrap_or(&hit.path);
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                format!("{}/", parent.display())
            }
            _ => String::new(),
        }
    }

    fn render_hit(&mut self, index: usize, hit: &Hit, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let selected = index == self.cursor;
        let location = self.location(hit);
        let detail = match &hit.line {
            Some(line) => format!("第 {} 行：{}", line.number, line.text),
            None => format!(
                "{} · {}",
                format::file_size(hit.size),
                format::modified(hit.modified)
            ),
        };

        h_flex()
            .id(("search-hit", index))
            .w_full()
            .h(px(ROW_HEIGHT))
            .gap_2()
            .items_center()
            .px_3()
            .rounded(radius)
            .cursor_pointer()
            .bg(if selected {
                cx.theme().muted
            } else {
                transparent_white()
            })
            .child(Icon::new(if hit.is_dir {
                IconName::Folder
            } else {
                IconName::File
            }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0()
                    .child(div().text_sm().truncate().child(hit.name.clone()))
                    .child(div().text_xs().text_color(muted).truncate().child(detail)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(240.))
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(location),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.reveal_at(index, cx)))
            .into_any_element()
    }
}

impl EventEmitter<SearchEvent> for SearchPanel {}

impl Render for SearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 点了"存为…"就把焦点交给名字输入框，用户接着敲名字、回车存下。
        if self.focus_name {
            self.focus_name = false;
            let handle = self.name_input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;

        let status = self.status();
        let has_error = self.error.is_some();
        let row_sizes = self.row_sizes.clone();
        let empty_hint = self.query.is_some() && !self.running && self.hits.is_empty();

        let body: AnyElement = if empty_hint {
            div()
                .id("search-results")
                .test_support()
                .aria_label("没有匹配的文件")
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("没有匹配的文件")
                .into_any_element()
        } else {
            // 结果可能几千条，走虚拟列表：只画看得见的那几行。
            let list = v_virtual_list(
                cx.entity().clone(),
                "hits",
                row_sizes,
                |panel, range, _, cx| {
                    range
                        .filter_map(|index| {
                            let hit = panel.hits.get(index)?.clone();
                            Some(panel.render_hit(index, &hit, cx))
                        })
                        .collect()
                },
            )
            .flex_1()
            .min_h_0();
            div()
                .id("search-results")
                .test_support()
                // 测试与读屏软件都靠这一句知道现在列出了什么。
                .aria_label(hit_summary(&self.hits))
                .flex_1()
                .min_h_0()
                .child(list)
                .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
                .into_any_element()
        };

        let root_label = self.root.display().to_string();
        let saved = crate::settings::Settings::read(cx).saved_searches.clone();

        // 存下来的搜索：一排小签，点一下就把查询填回输入框。
        // 之所以不做成"智能文件夹"（一个装着结果的假目录）：
        // 结果是活的，随时会变，假目录还得跟着刷新、还得拦住拖放和新建，
        // 而这里点一下就能看到同样一份结果，代价小得多。
        let saved_row = (!saved.is_empty()).then(|| {
            h_flex()
                .id("saved-searches")
                .test_support()
                .flex_wrap()
                .gap_2()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(border)
                .children(saved.iter().enumerate().map(|(index, item)| {
                    let name = item.name.clone();
                    let query = item.query.clone();
                    h_flex()
                        .id(format!("saved-search-{index}"))
                        .test_support()
                        .aria_label(format!("{name}：{query}"))
                        .gap_1()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded(radius)
                        .border_1()
                        .border_color(border)
                        .cursor_pointer()
                        .child(div().text_xs().child(name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .max_w(px(160.))
                                .truncate()
                                .child(query.clone()),
                        )
                        .child(
                            Button::new(format!("forget-search-{index}"))
                                .icon(IconName::Close)
                                .xsmall()
                                .ghost()
                                .tooltip("忘掉这个搜索")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // 别让这一下冒到签上：那会变成"填回查询"。
                                    cx.stop_propagation();
                                    this.forget(name.clone(), cx);
                                })),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_query(&query, window, cx)
                        }))
                }))
        });

        div()
            .id("search")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            // 键盘走遮罩自己的 `on_key_down`（冒泡阶段），不依赖快捷键表——
            // 输入框里的 ↑↓ 也要能挪光标，而 Enter 不该去打开别的东西。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // 面板开着的时候键盘归它：不放这一道闸，Cmd+C 会一边在输入框里
                // 复制文字、一边让窗格把选中文件拷进剪贴板。
                // 唯一放行的是开关面板那个键——按它等于收起面板。
                let toggle = crate::keymap::KeyMap::read(cx).key_for(PaneCommand::Search);
                if !toggle.is_empty() && event.keystroke.unparse() == toggle {
                    return;
                }
                let key = event.keystroke.key.as_str();
                // 面板自己要用的四个键必须吃掉：否则 ↑↓ 会同时挪窗格的光标，
                // Enter 会去打开窗格光标下的文件，Esc 会去清空选中。
                let ours = matches!(key, "up" | "down" | "enter" | "escape");
                if !ours {
                    // 剩下的键里，打字的和编辑的要放给输入框——
                    // `stop_propagation` 会连字符一起吞掉，输入框就打不进字了。
                    let modifiers = &event.keystroke.modifiers;
                    let editing = (modifiers.platform || modifiers.control)
                        && matches!(key, "c" | "v" | "x" | "a");
                    let typing = !modifiers.platform
                        && !modifiers.control
                        && !modifiers.alt
                        && !modifiers.function;
                    if editing || typing {
                        return;
                    }
                }
                cx.stop_propagation();
                match key {
                    "up" => this.move_cursor(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    // 正在起名字的时候 Enter 是"存下"，不是"跳到那个文件"。
                    "enter" if this.naming => this.save_current(window, cx),
                    "enter" => this.reveal_cursor(cx),
                    "escape" => cx.emit(SearchEvent::Close),
                    _ => {}
                }
            }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Close)))
            .child(
                v_flex()
                    .id("search-card")
                    .test_support()
                    .w(px(CARD_WIDTH))
                    .max_h(px(CARD_MAX_HEIGHT))
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(cx.theme().background)
                    // 点卡片里不该关掉面板：吃掉点击，别让它冒到遮罩上。
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(46.))
                            .border_b_1()
                            .border_color(border)
                            .child(Icon::new(IconName::Search).size_4().text_color(muted))
                            .child(Input::new(&self.input).cleanable(true).w_full())
                            .child(
                                Button::new("search-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .tooltip("关闭")
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Close)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("search-status")
                            .test_support()
                            .aria_label(status.clone())
                            .px_3()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(if has_error { cx.theme().danger } else { muted })
                            .child(status)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(muted)
                                    .truncate()
                                    .child(root_label),
                            )
                            .child(
                                // 子目录要不要进：搜 `node_modules` 里的东西是另一回事，
                                // 但找"就在这一层的文件"时钻下去只会添乱。
                                Button::new("search-depth")
                                    .label(if self.shallow {
                                        "只看这一层"
                                    } else {
                                        "含子目录"
                                    })
                                    .small()
                                    .ghost()
                                    .tooltip("切换要不要进子目录")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.shallow = !this.shallow;
                                        this.schedule(cx);
                                    })),
                            )
                            .child(
                                Button::new("search-save")
                                    .label("存为…")
                                    .small()
                                    .ghost()
                                    .tooltip("把这句查询存下来，起个名字")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.naming = true;
                                        // 焦点等下一帧再给：按钮自己在点击之后
                                        // 会把焦点收回去，当场给会被它盖掉。
                                        this.focus_name = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    // `Option<T>` 本身就是一个 0/1 个元素的迭代器，
                    // 所以"要不要这一行"直接交给 `children`。
                    .children(self.naming.then(|| {
                        h_flex()
                            .id("search-naming")
                            .test_support()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .items_center()
                            .child(Input::new(&self.name_input).w(px(220.)))
                            .child(
                                Button::new("search-save-confirm")
                                    .label("存下")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_current(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("search-save-cancel")
                                    .label("取消")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.naming = false;
                                        cx.notify();
                                    })),
                            )
                    }))
                    .child(body)
                    .children(saved_row)
                    .child(
                        h_flex()
                            .h(px(30.))
                            .px_3()
                            .gap_3()
                            .items_center()
                            .border_t_1()
                            .border_color(border)
                            .text_xs()
                            .text_color(muted)
                            .child("↑↓ 选择")
                            .child("Enter 跳到该文件")
                            .child("Esc 关闭"),
                    ),
            )
    }
}

/// 结果区的一句话摘要：给读屏软件，也给集成测试定位用。
fn hit_summary(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    hits.iter()
        .take(20)
        .map(|hit| hit.name.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}
