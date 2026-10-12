//! 一个浏览窗格：工具栏、文件列表、详情面板与状态栏。
//!
//! 窗格自己持有目录状态，这样以后加标签页时每个标签就是一个 `Entity<Pane>`。
//! 文件操作不在这里执行：窗格只向 [`Ops`] 发起请求，完成后根据事件刷新自己。

use std::{
    cell::Cell,
    collections::HashMap,
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
use crate::model::{Listing, Selection, Sort, SortKey, ViewMode};
use crate::ops_entity::{Ops, OpsEvent};
use crate::preview::{self, Preview};
use crate::settings::Settings;
use crate::terminal;
use crate::thumbnails::{self, Thumbnails, ThumbnailsEvent};
use crate::watch;

const ROW_HEIGHT: f32 = 40.;
const COL_SIZE: f32 = 100.;
const COL_MODIFIED: f32 = 130.;
const COL_KIND: f32 = 90.;

/// 网格里一个格子的尺寸（逻辑像素）。
const TILE_WIDTH: f32 = 112.;
const TILE_HEIGHT: f32 = 132.;
/// 格子之间的间距，也是网格的左右留白（`px_3`）。
const TILE_GAP: f32 = 12.;
/// 格子里的缩略图区边长。
const TILE_ART: f32 = 64.;
/// 宽度还没量出来时先按这个列数排，量到了立刻纠正。
const FALLBACK_COLUMNS: usize = 4;

/// 分列视图里一列的宽度与列间距。
///
/// 定宽而不是"几列平分"：Finder 那一系的分列视图都是定宽加横向滚动，
/// 平分会让深路径的每一列都窄到看不清名字。
const COLUMN_WIDTH: f32 = 200.;
const COLUMN_GAP: f32 = 1.;

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
        OpenTerminal,
        QuickLook,
        ViewList,
        ViewGrid,
        ViewColumns,
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
        KeyBinding::new("cmd-alt-t", OpenTerminal, context),
        // 空格是 Quick Look 的通用键位（macOS Finder、Dolphin 都是它）。
        KeyBinding::new("space", QuickLook, context),
        // 视图切换：Cmd+1/2 与工具栏那两颗按钮是同一件事。
        KeyBinding::new("cmd-1", ViewList, context),
        KeyBinding::new("cmd-2", ViewGrid, context),
        KeyBinding::new("cmd-3", ViewColumns, context),
    ]);
}

/// 窗格广播给应用的事件：当前目录变了，会话该存盘了。
#[derive(Clone, Debug)]
pub enum PaneEvent {
    Navigated,
    /// 这一栏被点了：双栏模式下应用据此把焦点挪过来。
    Focused,
}

impl EventEmitter<PaneEvent> for Pane {}

/// 窗格能执行的命令。命令面板、快捷键都走这一个入口，
/// 以后加“可配置快捷键”只需要在这里补一条，不用再散着接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneCommand {
    NewFolder,
    Rename,
    BulkRename,
    Trash,
    Copy,
    Cut,
    Paste,
    Undo,
    Refresh,
    GoUp,
    CopyPath,
    CopyFileName,
    OpenTerminal,
    QuickLook,
    /// 换成列表视图。
    ViewList,
    /// 换成网格视图。
    ViewGrid,
    /// 换成分列（Miller）视图。
    ViewColumns,
}

/// 分列视图里一行的全部信息。
///
/// 渲染闭包是 `'static` 的，借不到窗格里的 `Listing`，
/// 所以先把这一列要画的东西抄成一份可以搬进闭包的小结构。
#[derive(Clone)]
struct ColumnRow {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

/// 分列视图里一列的行与行高。
///
/// 行是 `Option`：`None` 表示"就是当前目录"，渲染时直接从 `listing` 取，
/// 不给十万项再抄一份。
type ColumnContent = (Option<Rc<Vec<ColumnRow>>>, Rc<Vec<Size<Pixels>>>);
/// 分列视图里一列的缓存内容：内容版本加上 [`ColumnContent`]。
type CachedColumn = (u64, Rc<Vec<ColumnRow>>, Rc<Vec<Size<Pixels>>>);

/// 分列视图里的一列：哪个目录、里面有什么、滚到哪儿了、哪一项是"通往下一层"的。
struct Column {
    path: PathBuf,
    /// 这一列要画的行。`None` 表示"就是当前目录"：渲染时直接从 `listing` 取，
    /// 不给十万项再抄一份缓存。
    rows: Option<Rc<Vec<ColumnRow>>>,
    /// 虚拟列表要的逐行高度。
    sizes: Rc<Vec<Size<Pixels>>>,
    /// 这一列自己的竖向滚动位置。最后一列用的是窗格那个，键盘要用。
    scroll: VirtualListScrollHandle,
    /// 通往下一层的那一项：祖先列里是路径上的下一级，最后一列里是光标所在项。
    active: Option<usize>,
    /// 是不是当前目录那一列。这一列的点击语义是"选中"，祖先列是"直接进去"。
    current: bool,
}

/// 一次 Quick Look 的快照。
///
/// 内容在**打开时**读一次就固定下来：预览浮层每帧都要画，
/// 每帧重新读一遍文件既浪费又会让滚动发抖。
///
/// 名字带 State 是因为 `QuickLook` 已经是那个空格键动作的名字了。
#[derive(Clone)]
struct QuickLookState {
    path: PathBuf,
    name: String,
    content: Preview,
}

pub struct Pane {
    ops: Entity<Ops>,
    /// 缩略图缓存由应用持有、所有标签共用：同一个文件不用每个标签各解一遍。
    thumbs: Entity<Thumbnails>,
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
    /// 虚拟列表要一份逐行高度，行数或行高变化时才重建。
    ///
    /// 列表模式下一项一行；网格模式下一"行"是 [`Self::columns`] 个格子。
    row_sizes: Rc<Vec<Size<Pixels>>>,
    /// 当前目录用什么视图显示。
    view: ViewMode,
    /// 量出来的列表可用宽度，网格据此算一行放几格。
    ///
    /// GPUI 没有元素级的 `on_resize`，也读不到兄弟元素的 bounds，
    /// 所以插一个零高度探针（[`Self::render_width_probe`]）把宽度带回实体。
    /// 探针是**上一帧**写的，因此宽度的变化会比窗口拉伸晚一帧跟上。
    width_probe: Rc<Cell<Pixels>>,
    known_width: Pixels,
    /// 分列视图里祖先列的行缓存：路径 -> (内容版本, 行, 行高)。
    ///
    /// 每帧都把十万项的名字逐个克隆一遍太贵，所以只在内容真的变了
    /// （换目录、刷新、改筛选、改排序）时才重抄一遍。
    column_rows_cache: HashMap<PathBuf, CachedColumn>,
    /// 目录内容的版本号：[`Self::sync_row_sizes`] 每跑一次就加一，
    /// 而它恰好在所有会改变行集合的地方都被调用过。
    listing_version: u64,
    /// 每一列自己的竖向滚动位置，下标就是列号（最后一列不在这里，用 `scroll`）。
    column_scrolls: HashMap<usize, VirtualListScrollHandle>,
    /// 分列视图整体的横向滚动位置。
    column_scroll: ScrollHandle,
    /// 上次横向滚到最右时的层数：层数没变就不再动它，
    /// 否则用户自己往左翻看祖先会被下一次重画拽回去。
    column_depth: usize,
    /// 当前目录的监听任务；丢掉它就等于停止监听。
    watch_task: Option<Task<()>>,
    /// Quick Look 浮层；`None` 就是没开。
    quick_look: Option<QuickLookState>,
    subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(
        path: PathBuf,
        ops: Entity<Ops>,
        thumbs: Entity<Thumbnails>,
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
            thumbs,
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
            view: ViewMode::default(),
            width_probe: Rc::new(Cell::new(px(0.))),
            known_width: px(0.),
            column_rows_cache: HashMap::new(),
            listing_version: 0,
            column_scrolls: HashMap::new(),
            column_scroll: ScrollHandle::new(),
            column_depth: 0,
            watch_task: None,
            quick_look: None,
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
        // 缩略图是后台生成的：图好了发一个事件，窗格重画一次就能看到。
        pane.subscriptions.push(cx.subscribe_in(
            &pane.thumbs,
            window,
            |this, _, event: &ThumbnailsEvent, _, cx| {
                let ThumbnailsEvent::Ready(path) = event;
                // 只在这个文件确实还看得见的时候才重画：后台任务回来的图，
                // 用户可能早就滚过去了。
                let visible = this.listing.entry(path).is_some()
                    || this.quick_look.as_ref().is_some_and(|look| look.path == *path);
                if visible {
                    cx.notify();
                }
            },
        ));

        // 先套用记住的视图：网格的一"行"是好几项，行高和行数都跟着变。
        pane.apply_remembered_view(cx);
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

    /// 当前选中数量。命令面板据此决定哪些命令值得列出来。
    pub fn selection_len(&self) -> usize {
        self.selection.len()
    }

    /// 把键盘焦点拿到这一栏来。应用切换双栏焦点时用它。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// 还有没有上一级可去（根目录就没有）。
    pub fn has_parent(&self) -> bool {
        self.listing.path().parent().is_some()
    }

    /// 套用这个目录上次记住的排序与视图；没去过就保持原样。
    fn apply_remembered_view(&mut self, cx: &mut Context<Self>) {
        if let Some(remembered) = Settings::read(cx).view_for(self.listing.path()) {
            self.listing.set_sort(remembered.sort);
            self.view = remembered.view;
        }
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

    /// 一行放得下几个格子。列表和分列恒为 1。
    fn columns(&self) -> usize {
        match self.view {
            ViewMode::List | ViewMode::Columns => 1,
            ViewMode::Grid => {
                if self.known_width <= px(0.) {
                    return FALLBACK_COLUMNS;
                }
                // 左右留白之外，每列还要吃掉一个间距（最后一列后面没有）。
                let usable = self.known_width.as_f32() - 2. * TILE_GAP + TILE_GAP;
                (usable / (TILE_WIDTH + TILE_GAP)).floor() as usize
            }
        }
        .max(1)
    }

    /// 第 `position` 项落在第几个虚拟行里。
    ///
    /// 滚动要按行算：网格里一行好几项，按项号滚会滚过头。
    fn row_of(&self, position: usize) -> usize {
        position / self.columns()
    }

    /// 虚拟列表一共要多少行。
    fn row_count(&self) -> usize {
        self.listing.visible_count().div_ceil(self.columns())
    }

    /// 一行多高：网格的行比列表高得多，中间还夹一个间距。
    fn row_height(&self) -> Pixels {
        match self.view {
            ViewMode::List | ViewMode::Columns => px(ROW_HEIGHT),
            ViewMode::Grid => px(TILE_HEIGHT + TILE_GAP),
        }
    }

    fn sync_row_sizes(&mut self) {
        // 分列视图的行缓存跟着这个版本号走：凡是会改变行集合的地方
        // （换目录、刷新、改筛选、改排序、换视图）都会走到这里。
        self.listing_version += 1;
        let rows = self.row_count();
        let height = self.row_height();
        let stale = self.row_sizes.len() != rows
            || self.row_sizes.first().is_some_and(|size| size.height != height);
        if stale {
            self.row_sizes = Rc::new(vec![size(px(0.), height); rows]);
        }
    }

    /// 换视图。排序和视图都记进设置：下次回到这个目录照旧。
    fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        // 换成分列时重新滚到最右：新视图第一次画，用户还没自己翻过。
        self.column_depth = 0;
        self.remember_view(cx);
        self.sync_row_sizes();
        // 视图换了，光标那一项的"第几行"也变了，把它滚回视野。
        if let Some(position) = self.selection.focus_position(&self.listing) {
            let row = self.row_of(position);
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// 把当前排序与视图写进设置。改排序和改视图都要走这里，
    /// 否则改完视图再改排序，视图会被写回默认值。
    fn remember_view(&mut self, cx: &mut Context<Self>) {
        let path = self.listing.path().to_path_buf();
        let sort = self.listing.sort();
        let view = self.view;
        Settings::update(cx, |settings| settings.remember_view(path, sort, view));
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
        self.go_to(path, remember, true, window, cx)
    }

    /// 换目录但不抢键盘焦点：双栏同步浏览时另一栏是被"带着走"的，
    /// 焦点该留在用户正在操作的那一栏——否则按一下 Enter 焦点就跳到对面去了。
    pub fn follow_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to(path, true, false, window, cx)
    }

    fn go_to(
        &mut self,
        path: PathBuf,
        remember: bool,
        take_focus: bool,
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
                // 这个目录上次用什么排序、什么视图就还用什么；
                // 没去过的地方沿用当前的。
                self.apply_remembered_view(cx);
                self.selection.clear();
                self.error = None;
                self.clear_filter(window, cx);
                self.sync_row_sizes();
                self.scroll_to_top();
                self.start_watching(cx);
                if take_focus {
                    self.focus.focus(window, cx);
                }
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
        // 记住这个目录的排序：下次回到这里还是这个顺序。
        self.remember_view(cx);
        self.sync_row_sizes();
        // 选中项按路径记，换排序之后还是原来那批文件；只把光标那行滚回视野。
        if let Some(position) = self.selection.focus_position(&self.listing) {
            let row = self.row_of(position);
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
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
        // 网格里一屏是"几行 × 每行几格"项，按行数翻只会翻过去一小截。
        let page = (rows.max(1) * self.columns()) as isize;
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
            .scroll_to_item(self.row_of(position), ScrollStrategy::Nearest);
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
    pub fn selected_paths(&self) -> Vec<PathBuf> {
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

    /// 终端该开在哪个目录：只选中一个目录时就开在那儿，否则开在当前目录。
    ///
    /// “我正看着这个文件夹，想在这底下敲命令”是最常见的意图，
    /// 而选中一个子目录时显然是想去那里面。
    fn terminal_target(&self) -> PathBuf {
        if self.selection.len() == 1
            && let Some(path) = self.selected_paths().into_iter().next()
            && path.is_dir()
        {
            return path;
        }
        self.listing.path().to_path_buf()
    }

    /// 在目标目录开一个终端窗口。失败时把原因写进状态栏，
    /// 不弹对话框——这类失败用户重试一次就知道，挡住界面不值得。
    fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let dir = self.terminal_target();
        let settings = Settings::read(cx);
        match terminal::open(&settings.terminal, &dir) {
            Ok(name) => {
                self.status = format!("已在{name}中打开 {}", dir.display());
            }
            Err(error) => {
                self.status = error;
            }
        }
        cx.notify();
    }

    /// 要看哪一项：光标所在的那一项，没有光标就退回唯一选中项。
    ///
    /// 多选时不预览——"多选然后按空格"用户通常是在勾选，不是想看五个文件。
    fn quick_look_target(&self) -> Option<PathBuf> {
        self.selection
            .cursor()
            .or_else(|| self.selection.single())
            .map(Path::to_path_buf)
    }

    /// 打开或关掉 Quick Look。已经在看同一个文件就关掉，
    /// 换了目标就直接切过去——连按空格在几个文件之间扫是常态。
    fn toggle_quick_look(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.quick_look_target() else {
            self.close_quick_look(cx);
            return;
        };
        if self.quick_look.as_ref().is_some_and(|look| look.path == path) {
            self.close_quick_look(cx);
            return;
        }

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // 内容在这时读一次：浮层每帧都画，每帧重读既浪费又会让滚动发抖。
        let content = preview::load(&path);
        self.quick_look = Some(QuickLookState {
            path,
            name,
            content,
        });
        cx.notify();
    }

    fn close_quick_look(&mut self, cx: &mut Context<Self>) {
        if self.quick_look.take().is_some() {
            cx.notify();
        }
    }

    /// Quick Look 开着没有。Esc 要先关它，再谈取消选中。
    pub fn quick_look_open(&self) -> bool {
        self.quick_look.is_some()
    }

    /// 命令面板据此决定要不要列出"快速查看"。
    pub fn quick_look_enabled(&self) -> bool {
        self.quick_look_target().is_some()
    }

    /// 执行一条命令。窗口参数是因为部分命令要弹对话框/移动焦点。
    pub fn run_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaneCommand::NewFolder => self.request_new_folder(cx),
            PaneCommand::Rename => self.request_rename(cx),
            PaneCommand::BulkRename => self.request_bulk_rename(cx),
            PaneCommand::Trash => self.request_trash(cx),
            PaneCommand::Copy => self.request_copy(cx),
            PaneCommand::Cut => self.request_cut(cx),
            PaneCommand::Paste => self.request_paste(cx),
            PaneCommand::Undo => self.request_undo(cx),
            PaneCommand::Refresh => self.refresh(cx),
            PaneCommand::GoUp => self.go_up(window, cx),
            PaneCommand::CopyPath => self.copy_paths(cx),
            PaneCommand::CopyFileName => self.copy_file_names(cx),
            PaneCommand::OpenTerminal => self.open_terminal(cx),
            PaneCommand::QuickLook => self.toggle_quick_look(cx),
            PaneCommand::ViewList => self.set_view(ViewMode::List, cx),
            PaneCommand::ViewGrid => self.set_view(ViewMode::Grid, cx),
            PaneCommand::ViewColumns => self.set_view(ViewMode::Columns, cx),
        }
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
            .children(self.render_view_buttons(cx))
            .child(
                Input::new(&self.filter)
                    .prefix(Icon::new(IconName::Search).size_4())
                    .cleanable(true)
                    .w(px(200.)),
            )
    }

    /// 视图切换：每种视图一颗按钮，当前那种亮着。
    ///
    /// 不用"一颗按钮轮流切"：那种要点好几下才知道自己切到哪儿了，
    /// 而这里总共就两种，直接点名字更快。
    fn render_view_buttons(&mut self, cx: &mut Context<Self>) -> Vec<Button> {
        ViewMode::ALL
            .iter()
            .map(|&mode| {
                let id = match mode {
                    ViewMode::List => "view-list",
                    ViewMode::Grid => "view-grid",
                    ViewMode::Columns => "view-columns",
                };
                Button::new(id)
                    .icon(Self::icon_for_view(mode))
                    .ghost()
                    .small()
                    .selected(self.view == mode)
                    .tooltip(format!("{}视图（{}）", mode.label(), mode.shortcut()))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_view(mode, cx)))
            })
            .collect()
    }

    fn icon_for_view(mode: ViewMode) -> IconName {
        match mode {
            ViewMode::List => IconName::List,
            ViewMode::Grid => IconName::LayoutGrid,
            ViewMode::Columns => IconName::Columns3,
        }
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
            Button::new("op-terminal")
                .icon(IconName::Terminal)
                .ghost()
                .small()
                .tooltip("在这里打开终端（Cmd+Alt+T）")
                // 换终端在命令面板里做：工具栏保持“按下去就打开”的单一职责。
                .on_click(cx.listener(|this, _, _, cx| this.open_terminal(cx))),
            Button::new("op-quick-look")
                .icon(IconName::Eye)
                .ghost()
                .small()
                .tooltip("快速查看（空格）")
                .disabled(self.quick_look_target().is_none())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_quick_look(cx))),
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
            // 让表头在集成测试里可被定位与点击。
            .test_support()
            .aria_label(label.to_string())
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
        // 图片显示缩略图，其余的用类型图标。
        // 第一次问会返回 `None` 并顺手登记一个后台任务，
        // 图好了缩略图层发事件，这一行自己就会重画——列表全程不卡顿。
        // 先克隆路径：`entry` 借的是 `self.listing`，问缩略图要 `&mut self`。
        let thumbnail_path = entry.path.clone();
        let thumbnail = if is_dir {
            None
        } else {
            self.thumbnail_of(&thumbnail_path, cx)
        };

        ListItem::new(("row", position))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_3()
            .py_0()
            .selected(is_selected)
            .accessibility_label(name.clone())
            .child(
                // `ListItem` 不是可观察元素，观测挂在内层容器上：
                // 集成测试靠它读出"第几行是哪个文件"，从而断言排序结果。
                h_flex()
                    .id(("row-name", position))
                    .test_support()
                    .aria_label(name.clone())
                    .w_full()
                    .gap_3()
                    .child(
                        div()
                            .w(px(20.))
                            .flex_shrink_0()
                            .text_color(icon_color)
                            .child(match thumbnail {
                                Some(thumbnail) => img(ImageSource::Render(thumbnail.image))
                                    .w(px(20.))
                                    .h(px(20.))
                                    .object_fit(ObjectFit::Contain)
                                    .rounded(px(3.))
                                    .into_any_element(),
                                None => Icon::new(if is_dir {
                                    IconName::Folder
                                } else {
                                    IconName::File
                                })
                                .size_4()
                                .into_any_element(),
                            }),
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
            return self.render_empty(cx);
        }

        let row_sizes = self.row_sizes.clone();
        v_flex()
            .id("file-list")
            // 让集成测试能一眼看出"现在是列表还是网格"，顺带给读屏软件报一声。
            .test_support()
            .aria_label("列表视图")
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

    /// 空目录（或筛选后一个都不剩）时的居中提示，两种视图共用。
    fn render_empty(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = if self.listing.query().is_empty() {
            (IconName::FolderOpen, "这个文件夹是空的")
        } else {
            (IconName::SearchX, "没有匹配的项目")
        };
        div()
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
            .into_any_element()
    }

    /// 网格：一个虚拟列表，但每一项是"一行格子"而不是一个文件。
    ///
    /// 之所以不直接用 `flex_wrap` 一把铺开：那样十万项会全部进布局树，
    /// 而 P2 的硬指标是十万项不卡。虚拟列表只画看得见的那几行。
    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.listing.visible_count() == 0 {
            return self.render_empty(cx);
        }

        let row_sizes = self.row_sizes.clone();
        let columns = self.columns();
        v_flex()
            .id("file-grid")
            .test_support()
            .aria_label("网格视图")
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                v_virtual_list(
                    cx.entity().clone(),
                    "rows",
                    row_sizes,
                    move |pane, visible_range, _, cx| {
                        visible_range
                            .map(|row| pane.render_tile_row(row, columns, cx))
                            .collect()
                    },
                )
                .track_scroll(&self.scroll)
                .p(px(TILE_GAP)),
            )
            .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
            .into_any_element()
    }

    /// 第 `row` 行：把这一行该有的格子横排出来，末尾不足就少放几个。
    fn render_tile_row(
        &mut self,
        row: usize,
        columns: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = row * columns;
        h_flex()
            .gap(px(TILE_GAP))
            .children((0..columns).filter_map(|column| self.render_tile(first + column, cx)))
            .into_any_element()
    }

    /// 一个格子：缩略图（或类型图标）、名字、大小。
    ///
    /// 名字那个内层 div 沿用列表行的 id（`row-name`）：集成测试同一套
    /// `row_names` 助手在两种视图下都能用，不用为网格再写一遍。
    fn render_tile(&mut self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self.listing.visible_entry(position)?;
        let path = entry.path.clone();
        let open_path = entry.path.clone();
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;
        let is_selected = self.selection.contains(&entry.path);
        // 到此 `entry` 不再被用到，它对 `self.listing` 的借用也就到头了：
        // 下面问缩略图要 `&mut self`，两笔借用不能重叠。
        let thumbnail = if is_dir {
            None
        } else {
            self.thumbnail_of_size(&path, thumbnails::GRID_EDGE, cx)
        };
        let caption = if is_dir {
            "文件夹".to_string()
        } else {
            format::file_size(size)
        };
        let icon_color = if is_dir {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };
        let radius = cx.theme().radius;
        let muted = cx.theme().muted_foreground;

        let tile = div()
            .id(("tile", position))
            // 集成测试靠格子之间的相对位置断言"一行放了几格"。
            .test_support()
            .w(px(TILE_WIDTH))
            .h(px(TILE_HEIGHT))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .p_1()
            .cursor_pointer()
            .rounded(radius)
            .child(
                div()
                    .w(px(TILE_ART))
                    .h(px(TILE_ART))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius)
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .text_color(icon_color)
                    .child(match thumbnail {
                        Some(thumbnail) => img(ImageSource::Render(thumbnail.image))
                            .size_full()
                            .object_fit(ObjectFit::Contain)
                            .into_any_element(),
                        None => Icon::new(if is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .size_10()
                        .into_any_element(),
                    }),
            )
            .child(
                div()
                    .id(("row-name", position))
                    .test_support()
                    .aria_label(name.clone())
                    .w_full()
                    .text_xs()
                    .text_center()
                    .line_clamp(2)
                    .child(name),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(caption),
            );

        Some(
            tile.when(is_selected, |tile| {
                tile.bg(cx.theme().muted)
                    .border_1()
                    .border_color(cx.theme().primary)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.select_row(path.clone(), event.modifiers);
                    this.update_status();
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
            .into_any_element(),
        )
    }

    /// 分列视图：从根目录一级一列铺下来，最后一列是当前目录。
    ///
    /// 横向不虚拟化——列数等于路径深度，一般不超过十列，
    /// 没有"十万列"这种场景；纵向每列各自虚拟化，深目录也扛得住。
    fn render_columns(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.build_columns();

        // 新展开的一列在最右边，不横向滚到底就看不见它。只在层数变化时才滚：
        // 否则用户往左翻看祖先时，随便一次重画都会把他拽回最右。
        if self.column_depth != columns.len() {
            self.column_depth = columns.len();
            let content = px(columns.len() as f32 * (COLUMN_WIDTH + COLUMN_GAP));
            let overflow = (content - self.known_width).max(px(0.));
            self.column_scroll.set_offset(point(-overflow, px(0.)));
        }

        h_flex()
            .id("columns")
            .test_support()
            .aria_label("分列视图")
            .flex_1()
            .min_h_0()
            .gap(px(COLUMN_GAP))
            .overflow_x_scroll()
            .track_scroll(&self.column_scroll)
            .horizontal_scrollbar(&self.column_scroll)
            .children(
                columns
                    .into_iter()
                    .enumerate()
                    .map(|(index, column)| self.render_column(index, column, cx)),
            )
            .into_any_element()
    }

    /// 从根目录到当前目录，每层一列。
    fn build_columns(&mut self) -> Vec<Column> {
        let mut chain: Vec<PathBuf> = self
            .listing
            .path()
            .ancestors()
            .map(Path::to_path_buf)
            .collect();
        chain.reverse();

        let sort = self.listing.sort();
        let cursor = self.selection.focus_position(&self.listing);
        let current_sizes = self.row_sizes.clone();
        let mut columns = Vec::with_capacity(chain.len());
        for (index, path) in chain.iter().enumerate() {
            let current = index + 1 == chain.len();
            // 当前那一列的行高表就是窗格自己那份：分列模式下两者算法一致。
            // 行不走缓存，渲染时直接从 `listing` 取。
            let (rows, sizes) = if current {
                (None, current_sizes.clone())
            } else {
                self.ancestor_column(path, sort)
            };
            // 祖先列各自记住自己滚到哪儿了；最后一列用窗格那个，键盘要用。
            let scroll = if current {
                self.scroll.clone()
            } else {
                self.column_scrolls
                    .entry(index)
                    .or_insert_with(VirtualListScrollHandle::new)
                    .clone()
            };
            let active = if current {
                cursor
            } else {
                // 通往下一层的那一项：就是在这一列里能找到下一级的那一行。
                // 通往下一层的那一项：在这一列里能找到下一级的那一行。
                let next = &chain[index + 1];
                rows.as_ref()
                    .and_then(|rows| rows.iter().position(|row| &row.path == next))
            };

            columns.push(Column {
                path: path.clone(),
                rows,
                sizes,
                scroll,
                active,
                current,
            });
        }
        columns
    }

    /// 一个祖先列要画的行与行高，按版本号缓存。
    ///
    /// 渲染闭包是 `'static` 的、借不到窗格里的 `Listing`，
    /// 所以先把这一列要画的东西抄成一份能搬进闭包的小结构。
    /// 每帧都抄一遍太贵（十万项的祖先目录会拖死界面），
    /// 于是只在目录内容真的变了时才重抄。
    fn ancestor_column(
        &mut self,
        path: &Path,
        sort: Sort,
    ) -> ColumnContent {
        if let Some((version, rows, sizes)) = self.column_rows_cache.get(path)
            && *version == self.listing_version
        {
            return (Some(rows.clone()), sizes.clone());
        }

        let mut listing =
            Listing::read(path).unwrap_or_else(|_| Listing::empty(path.to_path_buf()));
        // 跟着当前目录的排序走：不然两列之间顺序不一致，看着像换了内容。
        listing.set_sort(sort);
        let rows: Vec<ColumnRow> = (0..listing.visible_count())
            .filter_map(|position| listing.visible_entry(position))
            .map(|entry| ColumnRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
            })
            .collect();
        let sizes = Rc::new(vec![size(px(0.), px(ROW_HEIGHT)); rows.len()]);

        // 缓存不是无底洞：翻过的目录攒多了就整份丢掉，最坏是重读一次。
        if self.column_rows_cache.len() >= 64 {
            self.column_rows_cache.clear();
        }
        let rows = Rc::new(rows);
        self.column_rows_cache.insert(
            path.to_path_buf(),
            (self.listing_version, rows.clone(), sizes.clone()),
        );
        (Some(rows), sizes)
    }

    fn render_column(
        &mut self,
        index: usize,
        column: Column,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = column
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            // 根目录没有文件名，直接用它的写法（"/" 或 "C:\"）。
            .unwrap_or_else(|| column.path.display().to_string());
        let rows = column.rows.clone();
        let sizes = column.sizes.clone();
        let scroll = column.scroll.clone();
        let active = column.active;
        let current = column.current;

        v_flex()
            .id(("column", index))
            .test_support()
            .aria_label(title.clone())
            .w(px(COLUMN_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().background)
            .child(
                div()
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(title),
            )
            .child(
                v_virtual_list(
                    cx.entity().clone(),
                    ("column-rows", index),
                    sizes,
                    move |pane, visible_range, _, cx| {
                        visible_range
                            .map(|position| {
                                pane.render_column_row(&rows, position, active, current, cx)
                            })
                            .collect()
                    },
                )
                .track_scroll(&scroll)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// 分列里的一行。
    ///
    /// 祖先列点一下就进去（那才是分列视图的意义）；当前列点一下是选中，
    /// 双击才进——否则想多选几个文件都没法多选。
    fn render_column_row(
        &mut self,
        rows: &Option<Rc<Vec<ColumnRow>>>,
        position: usize,
        active: Option<usize>,
        current: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 当前那一列没有缓存：直接问窗格自己的 `listing` 要，省一份拷贝。
        let row = match rows {
            Some(rows) => rows.get(position).cloned(),
            None => self.listing.visible_entry(position).map(|entry| ColumnRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
            }),
        };
        let Some(row) = row else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let path = row.path.clone();
        let name = row.name.clone();
        let is_dir = row.is_dir;
        let is_active = active == Some(position);
        let is_selected = current && self.selection.contains(&path);
        let icon_color = if is_dir {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };

        div()
            .id(("column-row", position))
            .test_support()
            .aria_label(name.clone())
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .rounded(cx.theme().radius)
            .when(is_active || is_selected, |row| row.bg(cx.theme().muted))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(icon_color)
                    .child(Icon::new(if is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .size_4()),
            )
            .child(div().flex_1().min_w_0().truncate().child(name))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if !this.focus.is_focused(window) {
                        this.focus.focus(window, cx);
                    }
                    if !current {
                        // 祖先列：点文件夹就进去，点文件就打开。
                        if is_dir {
                            this.navigate_to(path.clone(), true, window, cx);
                        } else if let Err(error) = open::that(&path) {
                            this.error = Some(error.to_string());
                            this.update_status();
                            cx.notify();
                        }
                        return;
                    }
                    this.select_row(path.clone(), event.modifiers);
                    this.update_status();
                    let plain_double_click = event.click_count >= 2
                        && !event.modifiers.shift
                        && !event.modifiers.secondary();
                    if plain_double_click {
                        this.open_path(path.clone(), window, cx);
                    }
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// 量出"这一栏有多宽"。
    ///
    /// 网格要知道宽度才能算一行放几格，而 GPUI 既没有元素级的 `on_resize`，
    /// 也没有"读兄弟元素的 bounds"这种接口。于是插一个零高度的 canvas：
    /// 它布局完拿到自己的宽度（等于父容器可用宽度）写进共享单元格，
    /// 下一帧渲染时 [`Pane::render`] 读出来。零高度所以看不见，
    /// 窗口拉伸时每帧重排，宽度自己会跟上。
    fn render_width_probe(&self) -> AnyElement {
        let probe = self.width_probe.clone();
        canvas(
            move |bounds: Bounds<Pixels>, _, _| probe.set(bounds.size.width),
            |_, _, _, _| {},
        )
        .w_full()
        .h(px(0.))
        .into_any_element()
    }

    /// 表头 + 列表 + 状态栏，也就是键盘绑定生效的那一块。
    fn render_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            // id 带上实体号：双栏时两个窗格同屏，元素 id 必须各不相同，
            // 否则两个列表的状态（滚动位置、选中行）会互相打架。
            .id(("pane-body", cx.entity_id().as_u64() as usize))
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
                        cx.emit(PaneEvent::Focused);
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
            .on_action(cx.listener(|this, _: &OpenParent, window, cx| {
                this.run_command(PaneCommand::GoUp, window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| this.go_back(window, cx)))
            .on_action(cx.listener(|this, _: &GoForward, window, cx| this.go_forward(window, cx)))
            .on_action(cx.listener(|this, _: &Refresh, window, cx| {
                this.run_command(PaneCommand::Refresh, window, cx)
            }))
            // Esc 有优先顺序：先看有没有浮层要关，没有才轮到清空选中。
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.quick_look_open() {
                    this.close_quick_look(cx);
                } else {
                    this.clear_selection(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewFolder, window, cx| {
                this.run_command(PaneCommand::NewFolder, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Rename, window, cx| {
                this.run_command(PaneCommand::Rename, window, cx)
            }))
            .on_action(cx.listener(|this, _: &BulkRename, window, cx| {
                this.run_command(PaneCommand::BulkRename, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Trash, window, cx| {
                this.run_command(PaneCommand::Trash, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopySelection, window, cx| {
                this.run_command(PaneCommand::Copy, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CutSelection, window, cx| {
                this.run_command(PaneCommand::Cut, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyPath, window, cx| {
                this.run_command(PaneCommand::CopyPath, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyFileName, window, cx| {
                this.run_command(PaneCommand::CopyFileName, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Paste, window, cx| {
                this.run_command(PaneCommand::Paste, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Undo, window, cx| {
                this.run_command(PaneCommand::Undo, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenTerminal, window, cx| {
                this.run_command(PaneCommand::OpenTerminal, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLook, window, cx| {
                this.run_command(PaneCommand::QuickLook, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewList, window, cx| {
                this.run_command(PaneCommand::ViewList, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewGrid, window, cx| {
                this.run_command(PaneCommand::ViewGrid, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewColumns, window, cx| {
                this.run_command(PaneCommand::ViewColumns, window, cx)
            }))
            .child(self.render_width_probe())
            .when(self.view == ViewMode::List, |body| {
                body.child(self.render_header(cx))
            })
            .child(match self.view {
                ViewMode::List => self.render_list(cx),
                ViewMode::Grid => self.render_grid(cx),
                ViewMode::Columns => self.render_columns(cx),
            })
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
                    .id("pane-status")
                    .test_support()
                    .aria_label(self.status.clone())
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

impl Pane {
    /// Quick Look 浮层：居中卡片，内容按类型分派。
    ///
    /// **没做"点背景关掉"**：GPUI 的点击会冒泡到父元素，卡片里选中一段文字
    /// 也会被当成点了背景，一选就关太难受。关闭走 Esc、再按一次空格，
    /// 或者卡片底部的按钮。
    fn render_quick_look(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(look) = self.quick_look.clone() else {
            return div().into_any_element();
        };

        // 大图在这一帧问一次就够了，标题和正文共用。
        let thumbnail = self.thumbnail_of_size(&look.path, thumbnails::PREVIEW_EDGE, cx);
        let subtitle = Self::quick_look_subtitle(&look, thumbnail.clone());
        let body = match &look.content {
            Preview::Text(text) => Self::render_quick_look_text(text, cx),
            Preview::Hex(hex) => Self::render_quick_look_hex(hex, cx),
            Preview::Opaque(preview::Kind::Image) => {
                Self::render_quick_look_image(thumbnail.clone(), cx)
            }
            Preview::Opaque(kind) => Self::render_quick_look_placeholder(
                icon_for_kind(*kind),
                &look.name,
                cx,
            ),
            Preview::Directory => {
                Self::render_quick_look_placeholder(IconName::FolderOpen, &look.name, cx)
            }
            Preview::Unavailable(reason) => {
                Self::render_quick_look_placeholder(IconName::File, reason, cx)
            }
        };

        let muted = cx.theme().muted_foreground;
        div()
            .id("quick-look")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .child(
                v_flex()
                    .id("quick-look-card")
                    .w(px(720.))
                    .max_h(px(560.))
                    .gap_3()
                    .p_5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(
                        v_flex()
                            .gap_1()
                            .min_w_0()
                            .child(div().font_semibold().truncate().child(look.name.clone()))
                            .child(
                                div()
                                    .id("quick-look-subtitle")
                                    .test_support()
                                    .aria_label(subtitle.clone())
                                    .text_xs()
                                    .text_color(muted)
                                    .child(subtitle),
                            ),
                    )
                    .child(body)
                    .child(
                        h_flex()
                            .justify_end()
                            .child(
                                Button::new("quick-look-close")
                                    .label("关闭（Esc）")
                                    .ghost()
                                    .small()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.close_quick_look(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 卡片标题下面那一行：类型、规模，以及"这份不是全文"。
    fn quick_look_subtitle(look: &QuickLookState, thumbnail: Option<thumbnails::Thumbnail>) -> String {
        match &look.content {
            Preview::Text(text) => {
                let language = text.language.unwrap_or("文本");
                format!(
                    "{language} · {} 行{}",
                    text.lines,
                    if text.truncated { "（只显示开头）" } else { "" }
                )
            }
            Preview::Hex(hex) => format!(
                "二进制 · {} 字节（只显示前 {} 行）",
                hex.total_bytes,
                hex.rows.len()
            ),
            Preview::Opaque(kind) => match thumbnail {
                Some(thumbnail) => format!(
                    "{} · {} × {}",
                    kind.label(),
                    thumbnail.source_width,
                    thumbnail.source_height
                ),
                None => kind.label().to_string(),
            },
            Preview::Directory => "文件夹".to_string(),
            Preview::Unavailable(_) => "无法预览".to_string(),
        }
    }

    /// 文本/代码：等宽 + 不折行 + 自带滚动。
    fn render_quick_look_text(text: &preview::TextPreview, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        div()
            .id("quick-look-text")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .font_family(mono)
            .text_sm()
            .child(v_flex().children(
                text.text
                    .lines()
                    .map(|line| div().whitespace_nowrap().child(line.to_string())),
            ))
            .into_any_element()
    }

    /// 二进制：hex 转储，每行已经排好版。
    fn render_quick_look_hex(hex: &preview::HexPreview, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        div()
            .id("quick-look-hex")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .font_family(mono)
            .text_sm()
            .child(
                v_flex().children(
                    hex.rows
                        .iter()
                        .map(|row| div().whitespace_nowrap().child(row.format())),
                ),
            )
            .into_any_element()
    }

    /// 图片：还没生成好就先显示占位，图到了会收到事件再画一次。
    fn render_quick_look_image(
        thumbnail: Option<thumbnails::Thumbnail>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(thumbnail) = thumbnail else {
            return Self::render_quick_look_placeholder(
                IconName::GalleryVerticalEnd,
                "正在生成预览…",
                cx,
            );
        };

        div()
            .id("quick-look-image")
            .test_support()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .child(
                img(ImageSource::Render(thumbnail.image))
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::Contain)
                    .rounded(cx.theme().radius),
            )
            .into_any_element()
    }

    /// 没有内容可显示时的占位：一个图标加一句话。
    fn render_quick_look_placeholder(
        icon: IconName,
        message: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .h(px(220.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(icon).size_12())
            .child(div().text_sm().child(message.to_string()))
            .into_any_element()
    }

    /// 向缩略图层要一张图；列表里用小图，Quick Look 用大图。
    fn thumbnail_of(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<thumbnails::Thumbnail> {
        self.thumbnail_of_size(path, thumbnails::LIST_EDGE, cx)
    }

    fn thumbnail_of_size(
        &mut self,
        path: &Path,
        edge: u32,
        cx: &mut Context<Self>,
    ) -> Option<thumbnails::Thumbnail> {
        self.thumbs
            .update(cx, |thumbs, cx| thumbs.get(path, edge, cx))
    }
}

/// 每种类型用什么图标。没有专门图标的就退回通用文件图标。
fn icon_for_kind(kind: preview::Kind) -> IconName {
    match kind {
        preview::Kind::Image => IconName::GalleryVerticalEnd,
        preview::Kind::Pdf | preview::Kind::Text | preview::Kind::Code => IconName::FileText,
        preview::Kind::Audio | preview::Kind::Video => IconName::Play,
        preview::Kind::Archive => IconName::File,
        preview::Kind::Other => IconName::File,
    }
}

impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 探针写的是上一帧量到的宽度。网格的行数跟着列数走，
        // 所以宽度一变就得重排行高并再画一次；宽度稳定后自然收敛。
        let measured = self.width_probe.get();
        if measured > px(0.) && measured != self.known_width {
            self.known_width = measured;
            self.sync_row_sizes();
            cx.notify();
        }

        let mut root = h_flex()
            .size_full()
            .min_w_0()
            // 浮层是绝对定位的，父容器得先成为定位参照。
            .relative()
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
            .child(self.render_details(cx));

        if self.quick_look.is_some() {
            root = root.child(self.render_quick_look(cx));
        }
        root
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

