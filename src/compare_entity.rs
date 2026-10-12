//! 比较面板：两个目录差在哪，以及"同步"会动哪些文件。
//!
//! 算的活在 [`crate::compare`]，这里只管三件事：
//! 起任务、把差异摆成两栏对齐的列表、把同步按钮接上。
//!
//! 搬运**不走自己写的循环**：同步按目标目录归堆之后交给
//! [`crate::ops_entity::Ops`]，那边有进度、有冲突四策略、有撤销，
//! 再写一套等于把"可撤销"这个卖点重新实现一遍（还会实现错）。
//! 删除也走它的回收站，绝不自己删。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, Sizable as _, VirtualListScrollHandle,
    button::{Button, ButtonVariants as _},
    scroll::{ScrollableElement as _, ScrollbarAxis},
    v_virtual_list, *,
};
use gpui_kit::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Render, ScrollStrategy, Size,
    Subscription, Task, Window, div, px, size, transparent_white, *,
};

use crate::compare::{self, Entry, Options, Side, Stats, Status, SyncMode};
use crate::format;
use crate::ops::Control;
use crate::ops_entity::{Ops, OpsEvent};

const CARD_WIDTH: f32 = 860.;
const CARD_MAX_HEIGHT: f32 = 600.;
const ROW_HEIGHT: f32 = 40.;

/// 列表里看哪一类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// 所有差异。
    All,
    /// 只看两边都有但不一样的。
    Different,
    /// 只看只有一边有的。
    OneSide,
}

impl Filter {
    pub const ALL: [Self; 3] = [Self::All, Self::Different, Self::OneSide];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全部差异",
            Self::Different => "只看不同",
            Self::OneSide => "只看单边",
        }
    }

    fn keeps(self, entry: &Entry) -> bool {
        match self {
            Self::All => true,
            Self::Different => entry.status == Status::Different,
            Self::OneSide => matches!(entry.status, Status::LeftOnly | Status::RightOnly),
        }
    }
}

/// 面板告诉应用的两件事。
#[derive(Clone, Debug)]
pub enum CompareEvent {
    Close,
    /// 关掉面板并跳到这个文件。
    Reveal(PathBuf),
}

pub struct ComparePanel {
    ops: Entity<Ops>,
    options: Options,
    entries: Vec<Entry>,
    stats: Option<Stats>,
    filter: Filter,
    running: bool,
    control: Option<Control>,
    task: Option<Task<()>>,
    /// 第几趟。过期的批次靠它认出来。
    generation: usize,
    /// 同步之后的一句话交代（提交了多少、跳过了多少）。
    note: Option<String>,
    cursor: usize,
    scroll: VirtualListScrollHandle,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    focus: FocusHandle,
    /// 订阅得一直攥在手里，否则回调会被丢弃。
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl EventEmitter<CompareEvent> for ComparePanel {}

impl Focusable for ComparePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for ComparePanel {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ComparePanel {
    pub fn new(
        ops: Entity<Ops>,
        options: Options,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 文件一变就重比：同步完之后不用手动点"重算"。
        let subscription = cx.subscribe_in(&ops, window, |this, _, event: &OpsEvent, _, cx| {
            if matches!(event, OpsEvent::Changed { .. }) {
                this.start(cx);
            }
        });
        let mut this = Self {
            ops,
            options,
            entries: Vec::new(),
            stats: None,
            filter: Filter::All,
            running: false,
            control: None,
            task: None,
            generation: 0,
            note: None,
            cursor: 0,
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
            focus: cx.focus_handle(),
            subscriptions: vec![subscription],
        };
        this.start(cx);
        this
    }

    fn stop(&mut self) {
        if let Some(control) = self.control.take() {
            control.abort();
        }
        self.task = None;
        self.running = false;
    }

    /// 重比一趟：换口径、换筛选（筛选不用重扫描，但重算最省心）、
    /// 文件变了，都走这里。
    fn start(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.entries.clear();
        self.stats = None;
        self.cursor = 0;
        self.generation += 1;
        self.running = true;
        self.sync_rows();
        cx.notify();

        let generation = self.generation;
        let options = self.options.clone();
        let (tx, rx) = async_channel::unbounded::<compare::Event>();
        self.control = Some(compare::spawn(
            options,
            Arc::new(move |event| {
                tx.try_send(event).ok();
            }),
        ));
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        match event {
                            compare::Event::Batch(batch) => {
                                this.entries.extend(batch);
                                this.sync_rows();
                            }
                            compare::Event::Done(stats) => {
                                this.stats = Some(stats);
                                this.running = false;
                                this.control = None;
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

    /// 换比较口径（时间 / 内容）并重比。
    fn set_by_content(&mut self, by_content: bool, cx: &mut Context<Self>) {
        if self.options.by_content == by_content {
            return;
        }
        self.options.by_content = by_content;
        self.start(cx);
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.cursor = 0;
        self.sync_rows();
        cx.notify();
    }

    /// 当前筛选下看得见的那几条。
    fn visible(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|entry| self.filter.keeps(entry))
            .cloned()
            .collect()
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let count = self.visible().len();
        if count == 0 {
            return;
        }
        let moved = self.cursor as i64 + delta as i64;
        self.cursor = moved.clamp(0, count as i64 - 1) as usize;
        self.scroll
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter：跳到光标这一条（左边有就左边，没有就是右边的）。
    fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        let entries = self.visible();
        let Some(entry) = entries.get(self.cursor) else {
            return;
        };
        let path = entry
            .on(Side::Left, &self.options.left, &self.options.right)
            .or_else(|| entry.on(Side::Right, &self.options.left, &self.options.right));
        if let Some(path) = path {
            cx.emit(CompareEvent::Reveal(path));
        }
    }

    /// 同步：把计划交给 Ops 去搬，删除走回收站。
    ///
    /// 先算一份计划（纯数据），再按目标目录归堆——
    /// Ops 一次搬一批到同一个目录，于是一个目录一个任务，
    /// 进度、冲突处理、撤销都是原来那一套。
    fn sync(&mut self, mode: SyncMode, cx: &mut Context<Self>) {
        let plan = compare::plan(
            &self.visible(),
            &self.options.left,
            &self.options.right,
            mode,
        );
        if plan.is_empty() {
            self.note = Some("没有需要同步的东西".to_string());
            cx.notify();
            return;
        }
        let groups = compare::group_copies(plan.copies.clone());
        let copies = plan.copies.len();
        let mut batches = groups.len();
        for (dir, sources) in groups {
            // 目标目录可能还不存在（左边独有的整个子目录）：
            // 建目录是几毫秒的事，不值得为它再起一个任务。
            if let Err(error) = std::fs::create_dir_all(&dir) {
                self.note = Some(format!("建不了目录 {}: {}", dir.display(), error));
                cx.notify();
                return;
            }
            let ops = self.ops.clone();
            ops.update(cx, |ops, cx| ops.drop_into(sources, dir, false, cx));
        }

        let mut removed = 0usize;
        if !plan.removals.is_empty() {
            removed = plan.removals.len();
            batches += 1;
            let paths = plan.removals.clone();
            self.ops.update(cx, |ops, cx| ops.request_trash(paths, cx));
        }

        let note = if batches == 0 {
            "没有需要同步的东西".to_string()
        } else {
            format!("已交给操作队列：{copies} 个文件，分 {batches} 批")
        };
        let mut note = note;
        if removed > 0 {
            note.push_str(&format!(" · {removed} 项移进回收站（待确认）"));
        }
        if !plan.skipped.is_empty() {
            note.push_str(&format!(" · 跳过 {} 项（两边都改过）", plan.skipped.len()));
        }
        self.note = Some(note);
        cx.notify();
    }

    fn sync_rows(&mut self) {
        let count = self.visible().len();
        if self.row_sizes.len() == count {
            return;
        }
        self.row_sizes = Rc::new(vec![size(px(0.), px(ROW_HEIGHT)); count]);
    }

    /// 结果区上面那一行：正在比 / 比完了 / 同步的交代。
    fn status(&self) -> String {
        let mut text = match self.stats {
            Some(stats) => format!(
                "共 {} 处差异 · 左独有 {} · 右独有 {} · 不同 {} · 一样 {}",
                stats.diffs(),
                stats.left_only,
                stats.right_only,
                stats.different,
                stats.same
            ),
            None => {
                if self.running {
                    format!("正在比… 已列出 {}", self.entries.len())
                } else {
                    "没有差异".to_string()
                }
            }
        };
        if self.stats.is_some_and(|it| it.truncated) {
            text.push_str(" · 到上限了");
        }
        if let Some(note) = &self.note {
            text.push_str(&format!(" · {note}"));
        }
        text
    }

    fn name_of(path: &std::path::Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }

    fn render_row(&mut self, index: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let selected = index == self.cursor;
        let mut label = entry.status.label().to_string();
        match (entry.status, entry.newer) {
            (Status::Different, Some(Side::Left)) => label.push_str(" · 左新"),
            (Status::Different, Some(Side::Right)) => label.push_str(" · 右新"),
            // 两边都改过（时间分不出先后）：同步会跳过它，这里先说清楚。
            (Status::Different, None) => label.push_str(" · 分不出先后"),
            _ => {}
        }
        let path = entry.path.display().to_string();
        let left = side_text(entry.left.as_ref());
        let right = side_text(entry.right.as_ref());

        h_flex()
            .id(("compare-row", index))
            .test_support()
            .aria_label(format!("{label} {path}"))
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
            .child(
                div()
                    .w(px(96.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().text_sm().truncate().child(path))
            .child(
                div()
                    .w(px(150.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(left),
            )
            .child(
                div()
                    .w(px(150.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(right),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.cursor = index;
                cx.notify();
            }))
            .into_any_element()
    }
}

fn side_text(meta: Option<&compare::Meta>) -> String {
    match meta {
        Some(meta) => format!(
            "{} · {}",
            format::file_size(meta.bytes),
            format::modified(meta.modified)
        ),
        None => "—".to_string(),
    }
}

impl Render for ComparePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;
        let status = self.status();
        let row_sizes = self.row_sizes.clone();
        let visible = self.visible();

        let filters = h_flex()
            .gap_1()
            .children(Filter::ALL.iter().enumerate().map(|(index, filter)| {
                let filter = *filter;
                let label = filter.label();
                let tab = Button::new(("compare-filter", index))
                    .label(label)
                    .xsmall()
                    .tooltip(label.to_string());
                let tab = if filter == self.filter {
                    tab
                } else {
                    tab.ghost()
                };
                tab.on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }));

        let modes = h_flex().gap_1().children(
            [
                SyncMode::ToRight,
                SyncMode::ToLeft,
                SyncMode::BothWays,
                SyncMode::MirrorToRight,
                SyncMode::MirrorToLeft,
            ]
            .iter()
            .enumerate()
            .map(|(index, mode)| {
                let mode = *mode;
                let label = mode.label();
                Button::new(("compare-sync", index))
                    .label(label)
                    .xsmall()
                    .ghost()
                    .tooltip(label.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.sync(mode, cx)))
            }),
        );

        let body: AnyElement = if visible.is_empty() {
            div()
                .id("compare-rows")
                .test_support()
                .aria_label(status.clone())
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(if self.running {
                    "正在比…"
                } else {
                    "两个目录没有差异"
                })
                .into_any_element()
        } else {
            let list = v_virtual_list(
                cx.entity().clone(),
                "rows",
                row_sizes,
                |panel, range, _, cx| {
                    let entries = panel.visible();
                    range
                        .filter_map(|index| {
                            let entry = entries.get(index)?.clone();
                            Some(panel.render_row(index, &entry, cx))
                        })
                        .collect()
                },
            )
            .flex_1()
            .min_h_0();
            div()
                .id("compare-rows")
                .test_support()
                .aria_label(status.clone())
                .flex_1()
                .min_h_0()
                .child(list)
                .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
                .into_any_element()
        };

        let left_name = Self::name_of(&self.options.left);
        let right_name = Self::name_of(&self.options.right);

        div()
            .id("compare")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .track_focus(&self.focus)
            // 面板里有焦点时，窗格那套带上下文的快捷键匹配不上，
            // 这里就是唯一收得到键的地方（与搜索/分析面板同一条路）。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                cx.stop_propagation();
                match key {
                    "up" => this.move_cursor(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    "enter" => this.reveal_cursor(cx),
                    "escape" => cx.emit(CompareEvent::Close),
                    _ => {}
                }
            }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(CompareEvent::Close)))
            .child(
                v_flex()
                    .id("compare-card")
                    .test_support()
                    .w(px(CARD_WIDTH))
                    .max_h(px(CARD_MAX_HEIGHT))
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(cx.theme().background)
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(46.))
                            .border_b_1()
                            .border_color(border)
                            .child(Icon::new(IconName::Columns2).size_4().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_semibold()
                                    .truncate()
                                    .child(format!("比较：{left_name} ↔ {right_name}")),
                            )
                            .child(
                                Button::new("compare-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .tooltip("关闭")
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(CompareEvent::Close)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .items_center()
                            .border_b_1()
                            .border_color(border)
                            .child(filters)
                            .child(div().flex_1().min_w_0())
                            .child(
                                Button::new("compare-basis")
                                    .label(if self.options.by_content {
                                        "按内容比"
                                    } else {
                                        "按时间比"
                                    })
                                    .xsmall()
                                    .ghost()
                                    .tooltip("按内容（SHA-256）比最准，但要读两边的文件")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let next = !this.options.by_content;
                                        this.set_by_content(next, cx);
                                    })),
                            )
                            .child(
                                Button::new("compare-rerun")
                                    .label("重算")
                                    .xsmall()
                                    .ghost()
                                    .tooltip("再比一遍")
                                    .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                            ),
                    )
                    .child(
                        h_flex()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .items_center()
                            .border_b_1()
                            .border_color(border)
                            .child(div().text_xs().text_color(muted).child("同步："))
                            .child(modes),
                    )
                    .child(
                        h_flex()
                            .id("compare-status")
                            .test_support()
                            .aria_label(status.clone())
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(muted)
                            .child(div().flex_1().min_w_0().truncate().child(status)),
                    )
                    .child(body)
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
