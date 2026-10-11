//! 一个浏览窗格：工具栏、文件列表、详情面板与状态栏。
//!
//! 窗格自己持有目录状态，这样以后加标签页时每个标签就是一个 `Entity<Pane>`。
//! 文件操作不在这里执行：窗格只向 [`Ops`] 发起请求，完成后根据事件刷新自己。

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
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
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AsyncApp, Size, Task, WeakEntity, *};

use crate::clipboard;
use crate::format;
use crate::model::{Listing, Selection, Sort, SortKey};
use crate::ops_entity::{Ops, OpsEvent};
use crate::watch;

const ROW_HEIGHT: f32 = 40.;
const COL_SIZE: f32 = 100.;
const COL_MODIFIED: f32 = 130.;
const COL_KIND: f32 = 90.;

/// 键盘绑定的作用域。
///
/// 只有列表区在这个上下文里，工具栏的筛选框不在，
/// 所以在筛选框里打字、按方向键都不会误动文件列表。
const KEY_CONTEXT: &str = "WFilesPane";

/// 目录监听的去抖时间：批量改动收敛成一次刷新。
const WATCH_DEBOUNCE: Duration = Duration::from_millis(120);

gpui_kit::actions!(
    wfiles,
    [
        MoveUp,
        MoveDown,
        MoveFirst,
        MoveLast,
        PageUp,
        PageDown,
        SelectUp,
        SelectDown,
        SelectAll,
        Open,
        OpenParent,
        GoBack,
        GoForward,
        Refresh,
        ClearSelection,
        NewFolder,
        Rename,
        BulkRename,
        Trash,
        CopySelection,
        CutSelection,
        CopyPath,
        CopyFileName,
        Paste,
        Undo,
    ]
);

pub fn init(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, context),
        KeyBinding::new("down", MoveDown, context),
        KeyBinding::new("shift-up", SelectUp, context),
        KeyBinding::new("shift-down", SelectDown, context),
        KeyBinding::new("home", MoveFirst, context),
        KeyBinding::new("end", MoveLast, context),
        KeyBinding::new("pageup", PageUp, context),
        KeyBinding::new("pagedown", PageDown, context),
        KeyBinding::new("enter", Open, context),
        KeyBinding::new("cmd-down", Open, context),
        KeyBinding::new("backspace", OpenParent, context),
        KeyBinding::new("cmd-up", OpenParent, context),
        KeyBinding::new("cmd-[", GoBack, context),
        KeyBinding::new("cmd-]", GoForward, context),
        KeyBinding::new("cmd-r", Refresh, context),
        KeyBinding::new("cmd-a", SelectAll, context),
        KeyBinding::new("escape", ClearSelection, context),
        KeyBinding::new("cmd-shift-n", NewFolder, context),
        KeyBinding::new("f2", Rename, context),
        KeyBinding::new("cmd-shift-r", BulkRename, context),
        KeyBinding::new("cmd-backspace", Trash, context),
        KeyBinding::new("cmd-c", CopySelection, context),
        KeyBinding::new("cmd-x", CutSelection, context),
        KeyBinding::new("cmd-alt-c", CopyPath, context),
        KeyBinding::new("cmd-shift-c", CopyFileName, context),
        KeyBinding::new("cmd-v", Paste, context),
        KeyBinding::new("cmd-z", Undo, context),
    ]);
}

/// 窗格广播给应用的事件：当前目录变了，会话该存盘了。
#[derive(Clone, Debug)]
pub enum PaneEvent {
    Navigated,
}

impl EventEmitter<PaneEvent> for Pane {}

pub struct Pane {
    ops: Entity<Ops>,
    listing: Listing,
    selection: Selection,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    filter: Entity<InputState>,
    focus: FocusHandle,
    /// 最近一次读取目录失败的原因，显示在状态栏里。
    error: Option<String>,
    status: String,
    scroll: VirtualListScrollHandle,
    /// 虚拟列表要一份逐行高度，行数变化时才重建。
    row_sizes: Rc<Vec<Size<Pixels>>>,
    /// 当前目录的监听任务；丢掉它就等于停止监听。
    watch_task: Option<Task<()>>,
    subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(
        path: PathBuf,
        ops: Entity<Ops>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("筛选当前文件夹"));
        let (listing, error) = match Listing::read(&path) {
            Ok(listing) => (listing, None),
            Err(error) => (Listing::empty(path.clone()), Some(error)),
        };

        let mut pane = Self {
            ops,
            listing,
            selection: Selection::default(),
            back: Vec::new(),
            forward: Vec::new(),
            filter,
            focus: cx.focus_handle(),
            error,
            status: String::new(),
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
            watch_task: None,
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
        pane.subscriptions.push(cx.subscribe_in(
            &pane.ops,
            window,
            |this, _, event: &OpsEvent, _, cx| this.on_ops_event(event, cx),
        ));

        pane.sync_row_sizes();
        pane.update_status();
        pane.start_watching(cx);
        // 一打开就能用方向键，不需要先点一下列表。
        pane.focus.focus(window, cx);
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
        if !self.selection.is_empty() {
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
                let sort = self.listing.sort();
                self.listing = listing;
                self.listing.set_sort(sort);
                self.selection.clear();
                self.error = None;
                self.clear_filter(window, cx);
                self.sync_row_sizes();
                self.scroll_to_top();
                self.start_watching(cx);
                self.focus.focus(window, cx);
                cx.emit(PaneEvent::Navigated);
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
        let sort = self.listing.sort();

        match Listing::read(&path) {
            Ok(mut listing) => {
                listing.set_sort(sort);
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

    /// 关掉最后一个标签时把窗格重置回主页。
    pub fn reset_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.back.clear();
        self.forward.clear();
        self.navigate_to(path, false, window, cx);
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

    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.listing.toggle_sort(key);
        self.sync_row_sizes();
        // 选中项按路径记，换排序之后还是原来那批文件；只把光标那行滚回视野。
        if let Some(position) = self.selection.focus_position(&self.listing) {
            self.scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
        }
        self.update_status();
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.listing.visible_count() == 0 {
            return;
        }

        self.selection.select_all(&self.listing);
        self.update_status();
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }

        self.selection.clear();
        self.update_status();
        cx.notify();
    }

    fn open_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selection.cursor().map(PathBuf::from) else {
            return;
        };
        self.open_path(path, window, cx);
    }

    /// 方向键：`delta` 为负向上、为正向下；`extend` 是按住 Shift。
    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let count = self.listing.visible_count();
        if count == 0 {
            return;
        }

        let target = match self.selection.focus_position(&self.listing) {
            Some(position) => {
                let moved = (position as isize + delta).clamp(0, count as isize - 1);
                moved as usize
            }
            // 还没有光标时，向下从第一项开始，向上从最后一项开始。
            None => {
                if delta < 0 {
                    count - 1
                } else {
                    0
                }
            }
        };
        self.focus_position(target, extend, cx);
    }

    /// 整屏翻动，行数按当前视口能放多少算，窗口拉大拉小都跟着变。
    fn move_page(&mut self, direction: isize, cx: &mut Context<Self>) {
        let rows = self
            .scroll
            .bottom_item()
            .saturating_sub(self.scroll.top_item());
        let page = rows.max(1) as isize;
        self.move_cursor(page * direction, false, cx);
    }

    fn focus_position(&mut self, position: usize, extend: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.listing.visible_entry(position) else {
            return;
        };

        let path = entry.path.clone();
        if extend {
            self.selection.select_range(&self.listing, path);
        } else {
            self.selection.select(path);
        }
        self.scroll
            .scroll_to_item(position, ScrollStrategy::Nearest);
        self.update_status();
        cx.notify();
    }

    /// 点击选中：Shift 连选，Command（Windows/Linux 上是 Ctrl）加减选，普通点击单选。
    fn select_row(&mut self, path: PathBuf, modifiers: Modifiers) {
        if modifiers.shift {
            self.selection.select_range(&self.listing, path);
        } else if modifiers.secondary() {
            self.selection.toggle(path);
        } else {
            self.selection.select(path);
        }
    }

    // ---- 文件操作请求 ----

    /// 按列表顺序返回选中项，批量操作要用同一个顺序。
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selection.ordered(&self.listing)
    }

    fn request_new_folder(&mut self, cx: &mut Context<Self>) {
        let parent = self.listing.path().to_path_buf();
        self.ops
            .update(cx, |ops, cx| ops.request_new_folder(parent, cx));
    }

    fn request_rename(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.selection.single().map(PathBuf::from) {
            self.ops
                .update(cx, |ops, cx| ops.request_rename(path, cx));
        }
    }

    fn request_bulk_rename(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        self.ops
            .update(cx, |ops, cx| ops.request_bulk_rename(paths, cx));
    }

    fn request_trash(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        self.ops.update(cx, |ops, cx| ops.request_trash(paths, cx));
    }

    fn request_copy(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        self.ops.update(cx, |ops, cx| ops.copy(paths, cx));
    }

    fn request_cut(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        self.ops.update(cx, |ops, cx| ops.cut(paths, cx));
    }

    fn request_paste(&mut self, cx: &mut Context<Self>) {
        let dest = self.listing.path().to_path_buf();
        self.ops.update(cx, |ops, cx| ops.request_paste(dest, cx));
    }

    /// 复制完整路径：多个选中项每行一个，直接可粘到终端里。
    fn copy_paths(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(clipboard::clipboard_text(
            &paths,
            clipboard::ClipboardText::FullPath,
        )));
        self.status = format!("已复制 {} 个路径", paths.len());
        cx.notify();
    }

    /// 只复制文件名，适合贴到文档里列清单。
    fn copy_file_names(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(clipboard::clipboard_text(
            &paths,
            clipboard::ClipboardText::NameOnly,
        )));
        self.status = format!("已复制 {} 个文件名", paths.len());
        cx.notify();
    }

    fn request_undo(&mut self, cx: &mut Context<Self>) {
        self.ops.update(cx, |ops, cx| ops.request_undo(cx));
    }

    // ---- 目录监听与外部变更 ----

    /// 开始监听当前目录；切换目录时旧任务被丢弃，监听随之停止。
    fn start_watching(&mut self, cx: &mut Context<Self>) {
        let path = self.listing.path().to_path_buf();
        let Ok((watcher, rx)) = watch::watch_directory(&path) else {
            return;
        };

        self.watch_task = None;
        // async move 前缀形式 + 克隆 AsyncApp：future 持有所有权，跨 await 安全。
        self.watch_task = Some(cx.spawn(async move |pane: WeakEntity<Pane>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            // watcher 活在任务里：任务被丢弃（切换目录）时监听停止。
            let _watcher = watcher;
            while rx.recv().await.is_ok() {
                // 去抖：静默期内的后续事件全部读干，只刷新一次。
                cx.background_executor().timer(WATCH_DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                pane.update(&mut cx, |pane, cx| pane.refresh_if_watching(&path, cx))
                    .ok();
            }
        }));
    }

    /// 监听回调：确认还在看同一个目录才刷新（导航后的旧回调双保险）。
    fn refresh_if_watching(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.listing.path() == path {
            self.refresh(cx);
        }
    }

    fn on_ops_event(&mut self, event: &OpsEvent, cx: &mut Context<Self>) {
        // 剪贴板变化只影响按钮可用性，重绘时自然会读到最新状态。
        if !matches!(event, OpsEvent::Changed { .. }) {
            return;
        }
        cx.notify();
    }

    /// 文件操作或外部程序改了文件系统：能对上号就刷新，目录被搬走就跟着搬。
    pub fn apply_external_change(
        &mut self,
        removed: &[PathBuf],
        changed: &[PathBuf],
        renamed_to: &[(PathBuf, PathBuf)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.listing.path().to_path_buf();

        // 当前目录被搬走：跟着去新家，不记历史（前进后退是浏览用的）。
        if let Some((_, to)) = renamed_to.iter().find(|(from, _)| *from == path) {
            self.navigate_to(to.clone(), false, window, cx);
            return;
        }
        // 当前目录被删掉：刷新会落到错误状态，把原因说清楚。
        if removed.iter().any(|gone| gone == &path) {
            self.refresh(cx);
            return;
        }
        // 目录内容有变（里面新增、删除、改名，或者目录自己被改写）。
        let inside = |candidate: &PathBuf| candidate.starts_with(&path);
        if changed.iter().any(inside)
            || renamed_to
                .iter()
                .any(|(from, to)| inside(from) || inside(to))
        {
            self.refresh(cx);
        }
    }

    // ---- 渲染 ----

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.render_breadcrumb(cx))
            .child(
                Button::new("refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .tooltip("刷新")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .children(self.render_file_ops(cx))
            .child(
                Input::new(&self.filter)
                    .prefix(Icon::new(IconName::Search).size_4())
                    .cleanable(true)
                    .w(px(200.)),
            )
    }

    /// 文件操作按钮组：选中状态决定哪些能按。
    fn render_file_ops(&mut self, cx: &mut Context<Self>) -> Vec<Button> {
        let selected = self.selection.len();
        let can_write = self.error.is_none();
        let can_paste = !self.ops.read(cx).clipboard().paths.is_empty();

        vec![
            Button::new("op-new-folder")
                .icon(IconName::FolderPlus)
                .ghost()
                .small()
                .tooltip("新建文件夹（Cmd+Shift+N）")
                .disabled(!can_write)
                .on_click(cx.listener(|this, _, _, cx| this.request_new_folder(cx))),
            Button::new("op-rename")
                .icon(IconName::SquarePen)
                .ghost()
                .small()
                .tooltip("重命名（F2）")
                .disabled(selected != 1)
                .on_click(cx.listener(|this, _, _, cx| this.request_rename(cx))),
            Button::new("op-bulk-rename")
                .icon(IconName::TextCursorInput)
                .ghost()
                .small()
                .tooltip("批量重命名（Cmd+Shift+R）")
                .disabled(selected < 2)
                .on_click(cx.listener(|this, _, _, cx| this.request_bulk_rename(cx))),
            Button::new("op-trash")
                .icon(IconName::Trash)
                .ghost()
                .small()
                .tooltip("移到回收站（Cmd+Backspace）")
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.request_trash(cx))),
            Button::new("op-copy")
                .icon(IconName::Copy)
                .ghost()
                .small()
                .tooltip("拷贝（Cmd+C）")
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.request_copy(cx))),
            Button::new("op-cut")
                .icon(IconName::Scissors)
                .ghost()
                .small()
                .tooltip("剪切（Cmd+X）")
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.request_cut(cx))),
            Button::new("op-copy-path")
                .icon(IconName::Link)
                .ghost()
                .small()
                .tooltip("复制路径（Cmd+Alt+C）")
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.copy_paths(cx))),
            Button::new("op-copy-name")
                .icon(IconName::Signature)
                .ghost()
                .small()
                .tooltip("复制文件名（Cmd+Shift+C）")
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.copy_file_names(cx))),
            Button::new("op-paste")
                .icon(IconName::ClipboardPaste)
                .ghost()
                .small()
                .tooltip("粘贴（Cmd+V）")
                .disabled(!can_paste)
                .on_click(cx.listener(|this, _, _, cx| this.request_paste(cx))),
        ]
    }

    /// 面包屑：每一段都能点，段数太多时从左边裁掉，保证当前目录一直看得见。
    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.listing.path();
        let mut crumbs: Vec<(String, PathBuf)> = path
            .ancestors()
            .map(|ancestor| {
                let label = ancestor
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    // 根目录没有文件名，直接用它的写法（"/" 或 "C:\"）。
                    .unwrap_or_else(|| ancestor.display().to_string());
                (label, ancestor.to_path_buf())
            })
            .collect();
        crumbs.reverse();

        let last = crumbs.len().saturating_sub(1);
        let hover = cx.theme().muted;
        let current_color = cx.theme().foreground;
        let children = crumbs
            .into_iter()
            .enumerate()
            .map(|(index, (label, target))| {
                let is_current = index == last;
                let crumb = div()
                    .id(("crumb", index))
                    .h(px(22.))
                    .px_1()
                    .flex()
                    .items_center()
                    .rounded(cx.theme().radius)
                    .child(div().min_w_0().truncate().child(label));

                let crumb = if is_current {
                    crumb
                        .flex_shrink_0()
                        .font_medium()
                        .text_color(current_color)
                } else {
                    crumb
                        .min_w(px(36.))
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate_to(target.clone(), true, window, cx);
                        }))
                };

                h_flex()
                    .flex_shrink_0()
                    .min_w_0()
                    .gap_0()
                    .when(index > 0, |group| {
                        group.child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .child(Icon::new(IconName::ChevronRight).size_3()),
                        )
                    })
                    .child(crumb)
            });

        div()
            .h(px(32.))
            .flex_1()
            .min_w_0()
            .px_2()
            .flex()
            .items_center()
            .justify_end()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .children(children)
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(div().w(px(20.)).flex_shrink_0())
            .child(self.header_cell(0, SortKey::Name, "名称", None, cx))
            .child(self.header_cell(1, SortKey::Size, "大小", Some(COL_SIZE), cx))
            .child(self.header_cell(2, SortKey::Modified, "修改时间", Some(COL_MODIFIED), cx))
            .child(self.header_cell(3, SortKey::Kind, "类型", Some(COL_KIND), cx))
    }

    /// `width` 为 `None` 时这一列吃掉剩余宽度（名称列）。
    fn header_cell(
        &self,
        index: usize,
        key: SortKey,
        label: &str,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sort = self.listing.sort();
        let active = sort.key == key;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;

        let cell = div()
            .id(("header", index))
            .h_full()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .text_color(if active { foreground } else { muted })
            .hover(move |style| style.text_color(foreground))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_sort(key, cx)))
            .child(div().min_w_0().truncate().child(label.to_string()));

        let cell = if active {
            let icon = if sort.descending {
                IconName::ArrowDown
            } else {
                IconName::ArrowUp
            };
            cell.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .child(Icon::new(icon).size_3()),
            )
        } else {
            cell
        };

        match width {
            Some(width) => cell.w(px(width)).flex_shrink_0().into_any_element(),
            None => cell.flex_1().min_w_0().into_any_element(),
        }
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
                    this.select_row(path.clone(), event.modifiers);
                    this.update_status();
                    // 双击打开只在“干净”的点击下发生，带修饰键时用户是在多选。
                    let plain_double_click = event.click_count >= 2
                        && !event.modifiers.shift
                        && !event.modifiers.secondary();
                    if plain_double_click {
                        this.open_path(open_path.clone(), window, cx);
                    }
                    if !this.focus.is_focused(window) {
                        this.focus.focus(window, cx);
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

    /// 表头 + 列表 + 状态栏，也就是键盘绑定生效的那一块。
    fn render_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("pane-body")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            // 点空白处也要能把键盘焦点拿回来。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.focus.is_focused(window) {
                        this.focus.focus(window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(-1, false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.move_cursor(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.move_cursor(1, true, cx)))
            .on_action(cx.listener(|this, _: &MoveFirst, _, cx| this.focus_position(0, false, cx)))
            .on_action(cx.listener(|this, _: &MoveLast, _, cx| {
                let last = this.listing.visible_count().saturating_sub(1);
                this.focus_position(last, false, cx);
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_page(-1, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_page(1, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &Open, window, cx| this.open_cursor(window, cx)))
            .on_action(cx.listener(|this, _: &OpenParent, window, cx| this.go_up(window, cx)))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| this.go_back(window, cx)))
            .on_action(cx.listener(|this, _: &GoForward, window, cx| this.go_forward(window, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| this.clear_selection(cx)))
            .on_action(cx.listener(|this, _: &NewFolder, _, cx| this.request_new_folder(cx)))
            .on_action(cx.listener(|this, _: &Rename, _, cx| this.request_rename(cx)))
            .on_action(cx.listener(|this, _: &BulkRename, _, cx| this.request_bulk_rename(cx)))
            .on_action(cx.listener(|this, _: &Trash, _, cx| this.request_trash(cx)))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.request_copy(cx)))
            .on_action(cx.listener(|this, _: &CutSelection, _, cx| this.request_cut(cx)))
            .on_action(cx.listener(|this, _: &CopyPath, _, cx| this.copy_paths(cx)))
            .on_action(cx.listener(|this, _: &CopyFileName, _, cx| this.copy_file_names(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.request_paste(cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.request_undo(cx)))
            .child(self.render_header(cx))
            .child(self.render_list(cx))
            .child(self.render_status_bar(cx))
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
            None if self.selection.len() > 1 => {
                let selected = self.selection.ordered(&self.listing);
                let names: Vec<String> = selected
                    .iter()
                    .filter_map(|path| self.listing.entry(path))
                    .take(8)
                    .map(|entry| entry.name.clone())
                    .collect();
                let rest = selected.len().saturating_sub(names.len());

                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_semibold()
                            .child(format!("已选中 {} 项", selected.len())),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .text_sm()
                            .children(names.into_iter().map(|name| div().truncate().child(name)))
                            .when(rest > 0, |list| {
                                list.child(
                                    div()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("…… 还有 {rest} 项")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("批量操作会按列表顺序逐个应用。"),
                    )
                    .into_any_element()
            }
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
        let sort = self.listing.sort();
        h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
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
            .child(div().flex_shrink_0().child(format!(
                "按{}{}",
                sort_label(sort),
                if sort.descending { "降序" } else { "升序" }
            )))
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
                    .child(self.render_body(cx)),
            )
            .child(self.render_details(cx))
    }
}

fn sort_label(sort: Sort) -> &'static str {
    match sort.key {
        SortKey::Name => "名称",
        SortKey::Size => "大小",
        SortKey::Modified => "修改时间",
        SortKey::Kind => "类型",
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

