//! 一个浏览窗格：工具栏、文件列表、详情面板与状态栏。
//!
//! 窗格自己持有目录状态，这样以后加标签页时每个标签就是一个 `Entity<Pane>`。

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, VirtualListScrollHandle,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    scroll::{ScrollableElement as _, ScrollbarAxis},
    v_virtual_list, *,
};
// component::Size 是控件尺寸档位，这里要的是 gpui 的几何尺寸。
use gpui_kit::{Size, *};

use crate::format;
use crate::model::{Listing, Selection};

const ROW_HEIGHT: f32 = 40.;
const COL_SIZE: f32 = 100.;
const COL_MODIFIED: f32 = 130.;
const COL_KIND: f32 = 90.;

pub struct Pane {
    listing: Listing,
    selection: Selection,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    filter: Entity<InputState>,
    /// 最近一次读取目录失败的原因，显示在状态栏里。
    error: Option<String>,
    status: String,
    scroll: VirtualListScrollHandle,
    /// 虚拟列表要一份逐行高度，行数变化时才重建。
    row_sizes: Rc<Vec<Size<Pixels>>>,
    subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("筛选当前文件夹"));
        let (listing, error) = match Listing::read(&path) {
            Ok(listing) => (listing, None),
            Err(error) => (Listing::empty(path.clone()), Some(error)),
        };

        let mut pane = Self {
            listing,
            selection: Selection::default(),
            back: Vec::new(),
            forward: Vec::new(),
            filter,
            error,
            status: String::new(),
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
            subscriptions: Vec::new(),
        };

        pane.subscriptions.push(cx.subscribe_in(
            &pane.filter,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_filter_changed(cx);
                }
            },
        ));

        pane.sync_row_sizes();
        pane.update_status();
        pane
    }

    pub fn path(&self) -> &Path {
        self.listing.path()
    }

    fn on_filter_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).value().to_string();
        if self.listing.set_query(&query) {
            self.sync_row_sizes();
            self.scroll_to_top();
            self.update_status();
        }
        cx.notify();
    }

    fn sync_row_sizes(&mut self) {
        let count = self.listing.visible_count();
        if self.row_sizes.len() != count {
            self.row_sizes = Rc::new(vec![size(px(0.), px(ROW_HEIGHT)); count]);
        }
    }

    fn scroll_to_top(&self) {
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn update_status(&mut self) {
        if let Some(error) = &self.error {
            self.status = format!("无法打开：{error}");
            return;
        }

        let visible = self.listing.visible_count();
        let total = self.listing.total_count();
        let mut text = if visible == total {
            format!("{total} 个项目")
        } else {
            format!("{visible} / {total} 个项目")
        };
        if self.selection.len() > 0 {
            text.push_str(&format!("，已选中 {} 项", self.selection.len()));
        }
        self.status = text;
    }

    pub fn navigate_to(
        &mut self,
        path: PathBuf,
        remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match Listing::read(&path) {
            Ok(listing) => {
                if remember && path != self.listing.path() {
                    self.back.push(self.listing.path().to_path_buf());
                    self.forward.clear();
                }
                self.listing = listing;
                self.selection.clear();
                self.error = None;
                self.clear_filter(window, cx);
                self.sync_row_sizes();
                self.scroll_to_top();
            }
            Err(error) => {
                // 保留当前内容，只把失败说清楚，避免界面突然变空。
                self.error = Some(error);
            }
        }
        self.update_status();
        cx.notify();
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.listing.query().is_empty() {
            return;
        }

        self.listing.set_query("");
        self.filter
            .update(cx, |filter, cx| filter.set_value("", window, cx));
    }

    pub fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.back.pop() {
            self.forward.push(self.listing.path().to_path_buf());
            self.navigate_to(path, false, window, cx);
        }
    }

    pub fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.forward.pop() {
            self.back.push(self.listing.path().to_path_buf());
            self.navigate_to(path, false, window, cx);
        }
    }

    pub fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.listing.path().parent() {
            self.navigate_to(parent.to_path_buf(), true, window, cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let path = self.listing.path().to_path_buf();
        let query = self.listing.query().to_string();

        match Listing::read(&path) {
            Ok(mut listing) => {
                listing.set_query(&query);
                self.listing = listing;
                self.selection.retain_existing(&self.listing);
                self.error = None;
                self.sync_row_sizes();
            }
            Err(error) => self.error = Some(error),
        }
        self.update_status();
        cx.notify();
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let is_dir = self.listing.entry(&path).is_some_and(|entry| entry.is_dir);
        if is_dir {
            self.navigate_to(path, true, window, cx);
        } else if let Err(error) = open::that(&path) {
            self.error = Some(error.to_string());
            self.update_status();
            cx.notify();
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.listing.path().display().to_string();

        h_flex()
            .h(px(52.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("back")
                    .icon(IconName::ChevronLeft)
                    .ghost()
                    .tooltip("后退")
                    .disabled(self.back.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ChevronRight)
                    .ghost()
                    .tooltip("前进")
                    .disabled(self.forward.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.go_forward(window, cx))),
            )
            .child(
                Button::new("up")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .tooltip("上一级")
                    .disabled(self.listing.path().parent().is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
            )
            .child(
                div()
                    .h(px(32.))
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .text_sm()
                    .child(div().truncate().child(path)),
            )
            .child(
                Button::new("refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .tooltip("刷新")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                Input::new(&self.filter)
                    .prefix(Icon::new(IconName::Search).size_4())
                    .cleanable(true)
                    .w(px(220.)),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(32.))
            .px_3()
            .gap_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_xs()
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(div().w(px(20.)).flex_shrink_0())
            .child(div().flex_1().min_w_0().child("名称"))
            .child(div().w(px(COL_SIZE)).flex_shrink_0().child("大小"))
            .child(div().w(px(COL_MODIFIED)).flex_shrink_0().child("修改时间"))
            .child(div().w(px(COL_KIND)).flex_shrink_0().child("类型"))
    }

    fn render_row(&mut self, position: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.listing.visible_entry(position) else {
            return ListItem::new(("row", position))
                .w_full()
                .h(px(ROW_HEIGHT))
                .into_any_element();
        };

        let path = entry.path.clone();
        let open_path = entry.path.clone();
        let name = entry.name.clone();
        let kind = entry.kind_label();
        let is_dir = entry.is_dir;
        let is_selected = self.selection.contains(&entry.path);
        let icon_color = if is_dir {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };
        let muted = cx.theme().muted_foreground;
        let size_text = if is_dir {
            "—".to_string()
        } else {
            format::file_size(entry.size)
        };
        let modified_text = format::modified(entry.modified);

        ListItem::new(("row", position))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_3()
            .py_0()
            .selected(is_selected)
            .accessibility_label(name.clone())
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        div()
                            .w(px(20.))
                            .flex_shrink_0()
                            .text_color(icon_color)
                            .child(
                                Icon::new(if is_dir {
                                    IconName::Folder
                                } else {
                                    IconName::File
                                })
                                .size_4(),
                            ),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .w(px(COL_SIZE))
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(muted)
                            .child(size_text),
                    )
                    .child(
                        div()
                            .w(px(COL_MODIFIED))
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(muted)
                            .child(modified_text),
                    )
                    .child(
                        div()
                            .w(px(COL_KIND))
                            .flex_shrink_0()
                            .text_sm()
                            .truncate()
                            .text_color(muted)
                            .child(kind),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.selection.select(path.clone());
                    this.update_status();
                    if event.click_count >= 2 {
                        this.open_path(open_path.clone(), window, cx);
                    }
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.listing.visible_count() == 0 {
            let (icon, label) = if self.listing.query().is_empty() {
                (IconName::FolderOpen, "这个文件夹是空的")
            } else {
                (IconName::SearchX, "没有匹配的项目")
            };
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    v_flex()
                        .gap_3()
                        .items_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(icon).size_8())
                        .child(div().text_sm().child(label)),
                )
                .into_any_element();
        }

        let row_sizes = self.row_sizes.clone();
        v_flex()
            .id("file-list")
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                v_virtual_list(
                    cx.entity().clone(),
                    "rows",
                    row_sizes,
                    |pane, visible_range, _, cx| {
                        visible_range
                            .map(|position| pane.render_row(position, cx))
                            .collect()
                    },
                )
                .track_scroll(&self.scroll)
                .py_1(),
            )
            .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .selection
            .single()
            .and_then(|path| self.listing.entry(path));

        let content = match selected {
            Some(entry) => v_flex()
                .gap_4()
                .child(
                    div()
                        .h(px(120.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            Icon::new(if entry.is_dir {
                                IconName::FolderOpen
                            } else {
                                IconName::File
                            })
                            .size_12(),
                        ),
                )
                .child(div().font_semibold().truncate().child(entry.name.clone()))
                .child(detail_row("类型", entry.kind_label(), cx))
                .child(detail_row(
                    "大小",
                    if entry.is_dir {
                        "—".to_string()
                    } else {
                        format::file_size(entry.size)
                    },
                    cx,
                ))
                .child(detail_row("修改", format::modified(entry.modified), cx))
                .child(detail_row("位置", entry.path.display().to_string(), cx))
                .into_any_element(),
            None if self.selection.len() > 1 => v_flex()
                .gap_2()
                .child(
                    div()
                        .font_semibold()
                        .child(format!("已选中 {} 项", self.selection.len())),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("批量操作会按列表顺序逐个应用。"),
                )
                .into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::PanelRight).size_8())
                .child(div().text_sm().child("选择一个项目查看详情"))
                .into_any_element(),
        };

        v_flex()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .p_4()
            .child(div().pb_4().font_semibold().child("详细信息"))
            .child(content)
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(26.))
            .px_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.status.clone()),
            )
    }
}

impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                v_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_toolbar(cx))
                    .child(self.render_header(cx))
                    .child(self.render_list(cx))
                    .child(self.render_status_bar(cx)),
            )
            .child(self.render_details(cx))
    }
}

fn detail_row(label: &str, value: String, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(div().text_sm().truncate().child(value))
}
