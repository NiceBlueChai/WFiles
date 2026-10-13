//! 一个浏览窗格：工具栏、文件列表、详情面板与状态栏。
//!
//! 窗格自己持有目录状态，这样以后加标签页时每个标签就是一个 `Entity<Pane>`。
//! 文件操作不在这里执行：窗格只向 [`Ops`] 发起请求，完成后根据事件刷新自己。

use std::{
    any::Any,
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
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::{ScrollableElement as _, ScrollbarAxis},
    v_virtual_list, *,
};
// component::Size 是控件尺寸档位，这里要的是 gpui 的几何尺寸。
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AsyncApp, Size, Task, TestSupportExt as _, WeakEntity, *};

use crate::analyze_entity::{AnalyzeEvent, AnalyzeMode, AnalyzePanel};
use crate::archive;
use crate::clipboard;
use crate::drag::{self, DraggedPaths};
use crate::format;
use crate::keymap;
use crate::marks;
use crate::meta::{Metadata, MetadataEvent};
use crate::model::{FileEntry, Listing, Selection, Sort, SortKey, ViewMode};
use crate::ops_entity::{Ops, OpsEvent};
use crate::preview::{self, Preview};
use crate::remote;
use crate::remotes::{Remotes, RemotesEvent};
use crate::search_entity::{SearchEvent, SearchPanel};
use crate::settings::Settings;
use crate::terminal;
use crate::thumbnails::{self, Thumbnails, ThumbnailsEvent};
use crate::watch;

const ROW_HEIGHT: f32 = 40.;
const COL_SIZE: f32 = 100.;
/// 元数据列：六种（Git 状态 / 图片尺寸 / …）共用这一个位置，宽度也就一个。
const COL_META: f32 = 120.;
const COL_MODIFIED: f32 = 130.;
const COL_KIND: f32 = 90.;

/// 面包屑最多同时显示几段路径（不含那个「…」）。
///
/// 3 是量出来的，不是凑的：520 px 窗口收掉两侧之后容器只有 216 px，扣掉
/// 16 px 内边距、37 px 的「…」、以及当前目录自己的 60 px 保底，剩 103 px；
/// 两段祖先加两个 12px 箭头是 24×2+24 = 72 px，排得下，第三段就挤掉了。
/// 多显示一段祖先，不如把宽度留给"我在哪"。
const MAX_CRUMBS: usize = 3;

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
        Search,
        Mark,
        ViewList,
        ViewGrid,
        ViewColumns,
        // 压缩包这两条出厂不绑键（命令面板里用），但用户配了就得有对应动作。
        ViewArchive,
        CreateArchive,
        Analyze,
        EditRemote,
    ]
);

pub fn init(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    // 导航那一组固定不给改：上下左右、翻页、打开、前进后退、全选、Esc。
    // 它们是"在列表里走路"，跟滚动和选择绑在一起，改坏一个就走不动了；
    // 而且各家文件管理器这几条早就是肌肉记忆，没有理由让用户重学。
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
        KeyBinding::new("cmd-[", GoBack, context),
        KeyBinding::new("cmd-]", GoForward, context),
        KeyBinding::new("cmd-a", SelectAll, context),
        KeyBinding::new("escape", ClearSelection, context),
    ]);

    // 剩下的都走可配置那一套：出厂键 + 用户改过的，一次性绑好。
    let map = keymap::KeyMap::read(cx);
    let bindings = map
        .bindings()
        .into_iter()
        .filter_map(|(command, key)| key_binding_for(command, &key, context));
    cx.bind_keys(bindings);
}

/// 一条命令配一个键。`KeyBinding::new` 遇到解不开的写法会 panic，
/// 手改坏了的配置文件不能让应用起不来，所以解不开就跳过（那条回到没键）。
fn key_binding_for(
    command: PaneCommand,
    key: &str,
    context: Option<&'static str>,
) -> Option<KeyBinding> {
    if !keymap::is_usable(key) || Keystroke::parse(key).is_err() {
        return None;
    }
    Some(match command {
        PaneCommand::NewFolder => KeyBinding::new(key, NewFolder, context),
        PaneCommand::Rename => KeyBinding::new(key, Rename, context),
        PaneCommand::BulkRename => KeyBinding::new(key, BulkRename, context),
        PaneCommand::Trash => KeyBinding::new(key, Trash, context),
        PaneCommand::Copy => KeyBinding::new(key, CopySelection, context),
        PaneCommand::Cut => KeyBinding::new(key, CutSelection, context),
        PaneCommand::Paste => KeyBinding::new(key, Paste, context),
        PaneCommand::Undo => KeyBinding::new(key, Undo, context),
        PaneCommand::Refresh => KeyBinding::new(key, Refresh, context),
        PaneCommand::GoUp => KeyBinding::new(key, OpenParent, context),
        PaneCommand::CopyPath => KeyBinding::new(key, CopyPath, context),
        PaneCommand::CopyFileName => KeyBinding::new(key, CopyFileName, context),
        PaneCommand::OpenTerminal => KeyBinding::new(key, OpenTerminal, context),
        PaneCommand::QuickLook => KeyBinding::new(key, QuickLook, context),
        PaneCommand::Search => KeyBinding::new(key, Search, context),
        PaneCommand::Mark => KeyBinding::new(key, Mark, context),
        PaneCommand::ViewList => KeyBinding::new(key, ViewList, context),
        PaneCommand::ViewGrid => KeyBinding::new(key, ViewGrid, context),
        PaneCommand::ViewColumns => KeyBinding::new(key, ViewColumns, context),
        PaneCommand::ViewArchive => KeyBinding::new(key, ViewArchive, context),
        PaneCommand::CreateArchive => KeyBinding::new(key, CreateArchive, context),
        PaneCommand::Analyze => KeyBinding::new(key, Analyze, context),
        PaneCommand::EditRemote => KeyBinding::new(key, EditRemote, context),
    })
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
    /// 在当前目录下搜：连子目录、连文件内容一起找。
    Search,
    /// 给选中的那一项标个颜色、打个标签。
    Mark,
    /// 看看这个压缩包里有什么。
    ViewArchive,
    /// 把选中的东西打成一个 zip。
    CreateArchive,
    /// 算一算：文件夹多大、哪些文件重复、文件的校验和。
    Analyze,
    /// 换成列表视图。
    ViewList,
    /// 换成网格视图。
    ViewGrid,
    /// 换成分列（Miller）视图。
    ViewColumns,
    /// 编辑远程文件：下载到本地、系统打开，保存自动回传。
    EditRemote,
}

impl PaneCommand {
    /// 全部命令。配置层与命令面板都靠它枚举，加一条命令只改这一处。
    pub const ALL: [Self; 23] = [
        Self::NewFolder,
        Self::Rename,
        Self::BulkRename,
        Self::Trash,
        Self::Copy,
        Self::Cut,
        Self::Paste,
        Self::Undo,
        Self::Refresh,
        Self::GoUp,
        Self::CopyPath,
        Self::CopyFileName,
        Self::OpenTerminal,
        Self::QuickLook,
        Self::Search,
        Self::Mark,
        Self::ViewArchive,
        Self::CreateArchive,
        Self::ViewList,
        Self::ViewGrid,
        Self::ViewColumns,
        Self::Analyze,
        Self::EditRemote,
    ];

    /// 配置文件里用的名字。
    ///
    /// 不用 Rust 的变体名（`{:?}`）：那个写法一旦重命名变体就会变，
    /// 等于把内部实现的名字钉死在用户的磁盘上。
    pub fn id(self) -> &'static str {
        match self {
            Self::NewFolder => "new_folder",
            Self::Rename => "rename",
            Self::BulkRename => "bulk_rename",
            Self::Trash => "trash",
            Self::Copy => "copy",
            Self::Cut => "cut",
            Self::Paste => "paste",
            Self::Undo => "undo",
            Self::Refresh => "refresh",
            Self::GoUp => "go_up",
            Self::CopyPath => "copy_path",
            Self::CopyFileName => "copy_file_name",
            Self::OpenTerminal => "open_terminal",
            Self::QuickLook => "quick_look",
            Self::Search => "search",
            Self::Mark => "mark",
            Self::ViewArchive => "view_archive",
            Self::CreateArchive => "create_archive",
            Self::ViewList => "view_list",
            Self::ViewGrid => "view_grid",
            Self::ViewColumns => "view_columns",
            Self::Analyze => "analyze",
            Self::EditRemote => "edit_remote",
        }
    }

    /// 反查。配置文件是手改的，认不出来的名字要能报出来而不是崩。
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|command| command.id() == id)
    }

    /// 命令面板里搜这个词也能找到它。
    pub fn keywords(self) -> &'static str {
        match self {
            Self::NewFolder => "folder 创建 目录 快捷键 按键 keybinding",
            Self::Rename => "rename 改名 快捷键 按键 keybinding",
            Self::BulkRename => "batch 序号 模板 快捷键 按键 keybinding",
            Self::Trash => "delete 删除 快捷键 按键 keybinding",
            Self::Copy => "copy 快捷键 按键 keybinding",
            Self::Cut => "cut 移动 快捷键 按键 keybinding",
            Self::Paste => "paste 快捷键 按键 keybinding",
            Self::Undo => "undo 恢复 快捷键 按键 keybinding",
            Self::Refresh => "refresh 重新读取 快捷键 按键 keybinding",
            Self::GoUp => "up parent 上级 快捷键 按键 keybinding",
            Self::CopyPath => "path 剪贴板 快捷键 按键 keybinding",
            Self::CopyFileName => "name 剪贴板 快捷键 按键 keybinding",
            Self::OpenTerminal => "terminal 终端 shell 快捷键 按键 keybinding",
            Self::QuickLook => "preview 预览 查看 快捷键 按键 keybinding",
            Self::Search => "search 搜索 查找 find 内容 content 快捷键 按键 keybinding",
            Self::Mark => "mark 标记 标签 颜色 tag color 快捷键 按键 keybinding",
            Self::ViewArchive => "zip 压缩包 解压 快捷键 按键 keybinding",
            Self::CreateArchive => "zip 压缩包 打包 快捷键 按键 keybinding",
            Self::ViewList => "view list 列表 快捷键 按键 keybinding",
            Self::ViewGrid => "view grid 网格 缩略图 快捷键 按键 keybinding",
            Self::ViewColumns => "view miller 分列 层级 快捷键 按键 keybinding",
            Self::Analyze => "analyze 分析 大小 重复 校验和 md5 sha 快捷键 按键 keybinding",
            Self::EditRemote => "edit 编辑 remote 远程 回传 上传 快捷键 按键 keybinding",
        }
    }
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

/// 拖动时跟在鼠标后面那张小卡片。
///
/// 只有一个 Entity<W> 能被 GPUI 当成拖动预览画出来，
/// 所以这里哪怕只是几个字也得是一个视图。
struct DragPreview {
    label: String,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_sm()
            .text_color(cx.theme().foreground)
            .child(self.label.clone())
    }
}

/// 压缩包面板：在看哪个包、里面有什么、读没读出来。
///
/// 之所以不把压缩包当目录进：包里的条目没法复制、重命名、删，
/// 那些操作全得先解出来才有意义。老老实实给一个"看得见 + 解得出来"
/// 的面板，比给一堆点了会报错的按钮诚实。
#[derive(Clone)]
struct ArchiveState {
    path: PathBuf,
    name: String,
    entries: Vec<archive::Entry>,
    /// 读不出来时把原因摆在面板里，而不是弹一个对话框打断操作。
    error: Option<String>,
}

/// 正在给哪个文件做标记：路径、显示名、标签输入框。
#[derive(Clone)]
struct MarkState {
    path: PathBuf,
    name: String,
    input: Entity<InputState>,
}

/// 一次"弹簧文件夹"预约：鼠标停在哪个文件夹上、这一票是第几号。
///
/// 号票是关键：定时器起来的时候没法取消，只能靠对号判断——
/// 用户中途挪到别的文件夹上会开新的一票，旧票对不上号就自己作废。
struct SpringLoad {
    path: PathBuf,
    token: usize,
}

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
pub struct QuickLookState {
    pub path: PathBuf,
    pub name: String,
    pub content: Preview,
}

/// 菜单项回调的动作落回窗格自己。
///
/// [`WeakEntity::update`] 在窗格已经不在的时候返回 `Err`——标签被关掉、
/// 窗格被换掉都会这样。那不算错误：菜单能被点到说明点下去的那一帧窗格还在，
/// 真执行时没了就没有可执行的窗格，安静跳过是对的，不该往错误通道里报。
fn run_on_pane(
    me: &WeakEntity<Pane>,
    cx: &mut App,
    act: impl FnOnce(&mut Pane, &mut Context<Pane>),
) {
    let _ = me.update(cx, act);
}

pub struct Pane {
    /// 指向自己。
    ///
    /// 「更多」菜单里的每一项都得回头操作这一栏，而菜单项的回调签名
    /// 只有 `(&ClickEvent, &mut Window, &mut App)`——拿不到 `Context<Pane>`。
    /// 靠这个弱引用换回来：回调里 `update` 一下就行，而且弱引用不会
    /// 让这一栏在菜单开着的时候活不过来（强引用会）。
    /// 外面套一层 `Rc`：菜单项的 `on_click` 闭包要求 `'static`，
    /// 而 `WeakEntity` 不是 `Copy`——按引用捕获过不了生命周期，
    /// 按值捕获又会被外层那个 `Fn` 闭包吃掉。`Rc` 一次克隆就够，
    /// 里面只是复制一个弱引用指针，很便宜。
    me: Rc<WeakEntity<Self>>,
    ops: Entity<Ops>,
    /// 缩略图缓存由应用持有、所有标签共用：同一个文件不用每个标签各解一遍。
    thumbs: Entity<Thumbnails>,
    /// 元数据列的缓存与后台探测。
    meta: Entity<Metadata>,
    /// 远程连接的客户端与目录缓存。
    remotes: Entity<Remotes>,
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
    /// 拖动中鼠标正停在哪个文件夹上等着它"弹开"；`None` 就是没在等。
    spring: Option<SpringLoad>,
    /// 弹簧文件夹的号票计数器，只增不减，用来作废过期的定时器。
    spring_token: usize,
    /// 当前目录的监听任务；丢掉它就等于停止监听。
    watch_task: Option<Task<()>>,
    /// Quick Look 浮层；`None` 就是没开。
    quick_look: Option<QuickLookState>,
    /// 搜索面板；`None` 就是没开。
    search: Option<Entity<SearchPanel>>,
    /// 分析面板（大小 / 重复 / 校验和）；`None` 就是没开。
    analyze: Option<Entity<AnalyzePanel>>,
    /// 压缩包面板；`None` 就是没开。
    archive_view: Option<ArchiveState>,
    /// 正在后台打包/解压时把任务握在手里（丢掉即放弃），
    /// 顺带让按钮知道自己该灰着。
    archive_busy: Option<Task<()>>,
    /// 正在给哪个文件做标记（颜色与标签）；`None` 就是没在标。
    marking: Option<MarkState>,
    /// 正在给哪条命令录新按键；`None` 就是没在录。
    capturing: Option<PaneCommand>,
    /// 录按键浮层的焦点；录的时候焦点必须在这上面才收得到键。
    capture_focus: FocusHandle,
    /// 详情板（右侧那列预览）是否显示。
    ///
    /// 默认**显示**，但宽度可以收：这一列写死 260px 不可收缩，而工具栏
    /// 上一排按钮要 908px。窗口窄的时候两侧固定边框就把内容挤没了，
    /// 而详情板上的东西（预览、大小、EXIF）在快速浏览时并不是非看不可。
    pub(crate) details_shown: bool,
    subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(
        path: PathBuf,
        ops: Entity<Ops>,
        thumbs: Entity<Thumbnails>,
        meta: Entity<Metadata>,
        remotes: Entity<Remotes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::tr(cx, "筛选当前文件夹"))
        });
        let (listing, error) = match Listing::read(&path) {
            Ok(listing) => (listing, None),
            Err(error) => (Listing::empty(path.clone()), Some(error)),
        };

        let mut pane = Self {
            me: Rc::new(cx.weak_entity()),
            ops,
            thumbs,
            meta,
            remotes,
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
            spring: None,
            spring_token: 0,
            watch_task: None,
            quick_look: None,
            search: None,
            analyze: None,
            marking: None,
            archive_view: None,
            archive_busy: None,
            capturing: None,
            capture_focus: cx.focus_handle(),
            details_shown: true,
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
                    || this
                        .quick_look
                        .as_ref()
                        .is_some_and(|look| look.path == *path);
                if visible {
                    cx.notify();
                }
            },
        ));

        // 元数据是后台探出来的：值回来了/仓库状态回来了，重画一次就能看到。
        pane.subscriptions.push(cx.subscribe_in(
            &pane.meta,
            window,
            |this, _, event: &MetadataEvent, _, cx| match event {
                MetadataEvent::Ready(path) => {
                    // 只在这个文件还看得见的时候才重画。
                    if this.listing.entry(path).is_some() {
                        cx.notify();
                    }
                }
                // Git 一次调用回来一批，没法一个个对：
                // 但只重画确实在这个仓库里的窗格（双栏时另一栏可能在别处）。
                MetadataEvent::GitReady(root) => {
                    if this.listing.path().starts_with(root) {
                        cx.notify();
                    }
                }
            },
        ));

        // 远程目录是后台列出来的：回来了一批就填进当前目录。
        pane.subscriptions.push(cx.subscribe_in(
            &pane.remotes,
            window,
            |this, _, event: &RemotesEvent, _, cx| match event {
                RemotesEvent::Listed { path, error } => {
                    // 只填"现在正看着"的那一栏：用户可能早就切走了。
                    if *path != this.listing.path() {
                        return;
                    }
                    match error {
                        // 取回来了：换新的，"这是旧的"那层也就没意义了。
                        None => {
                            this.error = None;
                            this.fill_remote(path, cx);
                            this.update_status(cx);
                        }
                        // 没取回来：如果屏幕上还留着磁盘上那份，
                        // 就别把它换成一块错误页——用户要的是"昨天有什么"，
                        // 那份还在。错误另说，但别把内容抢走。
                        Some(reason) => {
                            let stale_at = this
                                .remotes
                                .update(cx, |remotes, _| remotes.staleness(path));
                            match stale_at {
                                Some(at) => {
                                    this.status = crate::i18n::tr_args(
                                        cx,
                                        "连不上服务器，显示 {ago} 列的这份",
                                        &[("ago", crate::offline::ago(at))],
                                    );
                                }
                                // 一份缓存都没有，就老实说连不上。
                                None => {
                                    this.error = Some(reason.clone());
                                    this.update_status(cx);
                                }
                            }
                        }
                    }
                    cx.notify();
                }
                // 回了传：服务器上的大小和时间都变了，正在看的那一栏该重问一次。
                RemotesEvent::Uploaded { error: None, .. } => {
                    if let Some((id, sub)) = remote::parse_location(this.listing.path()) {
                        this.remotes.update(cx, |remotes, cx| {
                            remotes.reload(&id, &sub, cx);
                        });
                    }
                    cx.notify();
                }
                RemotesEvent::Connected { .. }
                | RemotesEvent::Failed(_)
                | RemotesEvent::Editing { .. }
                | RemotesEvent::Uploaded { .. } => {}
            },
        ));

        // 先套用记住的视图：网格的一"行"是好几项，行高和行数都跟着变。
        pane.apply_remembered_view(cx);
        pane.sync_row_sizes();
        // 崩溃恢复：把上次选中的那些接上（行高定下来了才能往那儿滚）。
        pane.restore_selection(cx);
        pane.update_status(cx);
        pane.start_watching(cx);
        // 一打开就能用方向键，不需要先点一下列表。
        pane.focus.focus(window, cx);
        pane
    }

    /// 把崩溃前选中的文件接回来。
    ///
    /// **只恢复本地目录**：远程那一栏的列表是后台列出来的（`Listed` 事件
    /// 回来时才有条目），构造这一刻 `listing` 里一个条目都没有，恢复什么
    /// 都是空的。要接也得在 `fill_remote` 之后，那条路留到 P5后面的迭代。
    fn restore_selection(&mut self, cx: &mut Context<Self>) {
        let dir = self.listing.path().to_path_buf();
        if remote::parse_location(&dir).is_some() {
            return;
        }
        let Some(item) = crate::restore::RestoreSet::load().at(&dir).cloned() else {
            return;
        };
        // 只接住**这个目录里还在的**条目：`sanitized()` 已经滤过一遍
        // 磁盘上还在不在，这里再挡一道——同一个目录在两次启动之间
        // 又被改过也说不准。
        let selected: Vec<PathBuf> = item
            .selected
            .iter()
            .filter(|path| self.listing.entry(path).is_some())
            .cloned()
            .collect();
        let cursor = item
            .cursor
            .filter(|path| self.listing.entry(path).is_some());
        self.selection.select_many(selected, cursor);
        let cursor = self.selection.cursor().map(Path::to_path_buf);
        if let Some(cursor) = cursor
            && let Some(position) = self.listing.position_of(&cursor)
        {
            self.scroll.scroll_to_item(position, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// 记下这一栏的现场（选中项 + 光标），供崩溃后恢复。
    ///
    /// 什么时候记：选择变了就记。不是每次按键都记——那是一次写盘，
    /// 而按住 Shift 连选一串的时候一次按键能触发几十次。
    pub fn note_restore(&self) {
        let dir = self.listing.path().to_path_buf();
        if remote::parse_location(&dir).is_some() {
            // 远程的选中项记了也没地方恢复（见 `restore_selection`），
            // 写一份永远用不上的数据只是白白攒文件。
            return;
        }
        let selected = self.selection.ordered(&self.listing);
        let cursor = self.selection.cursor().map(Path::to_path_buf);
        let mut set = crate::restore::RestoreSet::load();
        set.note(dir, selected, cursor);
        if let Err(error) = set.save() {
            eprintln!("保存恢复现场失败：{error}");
        }
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
            self.update_status(cx);
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
            || self
                .row_sizes
                .first()
                .is_some_and(|size| size.height != height);
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

    /// 状态栏那句话。在事件里算好缓存进 `self.status`，不是渲染时现算。
    ///
    /// 收 `cx` 是为了按当前语言显示。
    ///
    /// **缓存带来的后果**：切语言之后状态栏要等下一次状态变化才刷新
    /// （选中项变了、目录换了才会调这里）。语言切换走的是"整窗重画"，
    /// 而状态栏读的是这个缓存字段，所以切语言那一帧状态栏可能还是旧语言。
    /// 记在这儿，别以为是 bug 就去重算。
    fn update_status(&mut self, cx: &gpui_kit::App) {
        if let Some(error) = &self.error {
            self.status =
                crate::i18n::tr_args(cx, "无法打开：{error}", &[("error", error.clone())]);
            return;
        }

        let visible = self.listing.visible_count();
        let total = self.listing.total_count();
        let mut text = if visible == total {
            crate::i18n::tr_args(cx, "{total} 个项目", &[("total", total.to_string())])
        } else {
            crate::i18n::tr_args(
                cx,
                "{visible} / {total} 个项目",
                &[
                    ("visible", visible.to_string()),
                    ("total", total.to_string()),
                ],
            )
        };
        // 远程位置要写明：列表长得跟本地一样，不说的话用户会以为
        // 自己在本地目录里，删改起来心里没底。
        if let Some((_, sub)) = remote::parse_location(self.listing.path()) {
            text = if sub.is_empty() {
                crate::i18n::tr_args(cx, "远程位置 · {text}", &[("text", text)])
            } else {
                crate::i18n::tr_args(
                    cx,
                    "远程位置 · {sub} · {text}",
                    &[("sub", sub.clone()), ("text", text)],
                )
            }
        }
        // 在云盘的同步文件夹里也要写明：这里删改会牵动另一台机器上的同一份，
        // 不说的话用户以为自己在动一个普通本地目录。
        if let Some(root) = crate::cloud::root_for(self.listing.path(), crate::cloud::detected()) {
            text = crate::i18n::tr_args(
                cx,
                "{provider} · {text}",
                &[
                    ("provider", root.provider.name().to_string()),
                    ("text", text),
                ],
            );
        }
        if !self.selection.is_empty() {
            // 逗号前缀写在模板里：中文这句要逗号断开，英文是直接跟在后面的
            // （", 3 selected"）。这个差别就是它该进模板而不该进代码的原因。
            text.push_str(&crate::i18n::tr_args(
                cx,
                "，已选中 {count} 项",
                &[("count", self.selection.len().to_string())],
            ));
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
        // 离开之前把这一栏的现场记下来：`selection.clear()` 就在下面，
        // 走了就再也补不回来了。
        if path != self.listing.path() {
            self.note_restore();
        }
        // 远程位置（dav://…）：本地那一套读不了，走后台那一支。
        if remote::parse_location(&path).is_some() {
            self.go_remote(path, remember, take_focus, window, cx);
            return;
        }
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
        self.update_status(cx);
        cx.notify();
    }

    /// 进一个远程目录：先把界面切过去（空列表），后台列出来再填。
    ///
    /// 之所以不"等列完再切"：那会让窗口在慢服务器上僵住几十秒，
    /// 而用户连卡在哪儿都不知道。先切过去、写明在取，才是能用的样子。
    fn go_remote(
        &mut self,
        path: PathBuf,
        remember: bool,
        take_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, sub)) = remote::parse_location(&path) else {
            return;
        };
        if remember && path != self.listing.path() {
            self.back.push(self.listing.path().to_path_buf());
            self.forward.clear();
        }
        self.listing = Listing::empty(path.clone());
        self.selection.clear();
        self.error = None;
        self.clear_filter(window, cx);
        self.sync_row_sizes();
        self.scroll_to_top();
        // 远程目录没有"文件变了"的通知可听，监听本地文件系统只会白跑。
        self.watch_task = None;
        if take_focus {
            self.focus.focus(window, cx);
        }

        let entries = self
            .remotes
            .update(cx, |remotes, cx| remotes.list(&id, &sub, cx));
        // 有没有垫上磁盘上那份，此刻还不知道——要等后台那一趟回来，
        // 服务器答了还是没答，"这是旧的"才说得准。所以这里不写状态栏，
        // 交给 RemotesEvent::Listed 那头定。
        match entries {
            // 已经列过了（内存或磁盘上有）：当场填进去。
            Some(_) => {
                self.fill_remote(&path, cx);
                self.update_status(cx);
            }
            // 后台正在取：先说清楚在等什么，别让人以为这是个空目录。
            None => self.status = crate::i18n::tr(cx, "正在取这个目录…"),
        }
        cx.emit(PaneEvent::Navigated);
        cx.notify();
    }

    /// 把后台取回来的那批条目装进当前目录。
    fn fill_remote(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some((id, _)) = remote::parse_location(path) else {
            return;
        };
        let entries = self
            .remotes
            .update(cx, |remotes, _| remotes.entries(path).cloned())
            .unwrap_or_default();
        let query = self.listing.query().to_string();
        let sort = self.listing.sort();
        let items = entries
            .into_iter()
            .map(|entry| {
                let child = remote::location(&id, &entry.path);
                // 服务器给的是 Unix 秒；没有时间就当没有（那一列空着）。
                let modified = entry
                    .modified
                    .map(|seconds| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
                FileEntry::remote(child, &entry.name, entry.is_dir, entry.size, modified)
            })
            .collect();
        let mut listing = Listing::from_entries(path.to_path_buf(), items);
        listing.set_sort(sort);
        listing.set_query(&query);
        self.listing = listing;
        self.selection.retain_existing(&self.listing);
        self.error = None;
        self.sync_row_sizes();
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
        // 卷会热插拔，而插拔事件三个平台给得都不一样（见 `volumes`
        // 模块注释），所以靠"用户明确刷了一下"来重扫。放在这儿而不是
        // 只挂在命令面板上：Cmd+R 走的是窗格自己的 `on_action`，
        // 挂 app 层的话这条最常用的路反而漏掉了。
        crate::volumes::rescan_now();
        let path = self.listing.path().to_path_buf();
        // 远程目录刷新 = 重新问一次服务器（缓存丢掉，别拿旧的糊弄）。
        if let Some((id, sub)) = remote::parse_location(&path) {
            self.remotes
                .update(cx, |remotes, cx| remotes.reload(&id, &sub, cx));
            self.status = crate::i18n::tr(cx, "正在取这个目录…");
            cx.notify();
            return;
        }
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
        self.update_status(cx);
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
        } else if remote::parse_location(&path).is_some() {
            // 远程文件：先下载到临时目录，再交给系统打开。
            let Some((id, sub)) = remote::parse_location(&path) else {
                return;
            };
            self.remotes
                .update(cx, |remotes, cx| remotes.open_file(&id, &sub, cx));
        } else if archive::is_archive(&path) {
            // 压缩包不交给系统：自己就能看得见里面，还能解得出来。
            self.open_archive(path, cx);
        } else if let Err(error) = open::that(&path) {
            self.error = Some(error.to_string());
            self.update_status(cx);
            cx.notify();
        }
    }

    /// 「编辑远程文件」此刻有没有意义：选中一个远程的、非目录的项。
    ///
    /// 命令面板用它决定列不列这条命令——在本地目录里按 Cmd+Alt+E
    /// 不该弹一个"不在这里"的错误。
    pub fn edit_remote_enabled(&self) -> bool {
        Self::editable_remote(&self.selection, &self.listing).is_some()
    }

    /// 选中项里有没有一个"能编辑的远程文件"：是就给出 `(连接 id, 服务器上的路径)`。
    ///
    /// 不写成方法（`&self`）：这样它不碰界面状态，喂一批路径进去就能验。
    fn editable_remote(selection: &Selection, listing: &Listing) -> Option<(String, String)> {
        let path = selection.single()?;
        let (id, sub) = remote::parse_location(path)?;
        // 目录没有"内容"可编辑。
        if listing.entry(path).is_some_and(|entry| entry.is_dir) {
            return None;
        }
        Some((id, sub))
    }

    /// 编辑远程文件（命令）：下载到本地临时目录、系统打开，保存自动回传。
    ///
    /// 只对选中**一个**远程文件生效：多选、目录、本地文件都不在语义里，
    /// 与其默默什么都不做，不如把原因写进状态栏。
    fn edit_remote(&mut self, cx: &mut Context<Self>) {
        let Some((id, sub)) = Self::editable_remote(&self.selection, &self.listing) else {
            self.error = Some(crate::i18n::tr(cx, "先选中一个远程文件"));
            self.update_status(cx);
            cx.notify();
            return;
        };
        self.remotes
            .update(cx, |remotes, cx| remotes.edit_file(&id, &sub, cx));
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
        self.update_status(cx);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.listing.visible_count() == 0 {
            return;
        }

        self.selection.select_all(&self.listing);
        self.note_restore();
        self.update_status(cx);
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }

        self.selection.clear();
        self.note_restore();
        self.update_status(cx);
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
        self.note_restore();
        self.update_status(cx);
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
        self.note_restore();
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
            self.ops.update(cx, |ops, cx| ops.request_rename(path, cx));
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
        self.status = crate::i18n::tr_args(
            cx,
            "已复制 {count} 个路径",
            &[("count", paths.len().to_string())],
        );
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
        self.status = crate::i18n::tr_args(
            cx,
            "已复制 {count} 个文件名",
            &[("count", paths.len().to_string())],
        );
        cx.notify();
    }

    fn request_undo(&mut self, cx: &mut Context<Self>) {
        self.ops.update(cx, |ops, cx| ops.request_undo(cx));
    }

    /// 终端该开在哪个目录：只选中一个目录时就开在那儿，否则开在当前目录。
    ///
    /// “我正看着这个文件夹，想在这底下敲命令”是最常见的意图，
    /// 而选中一个子目录时显然是想去那里面。
    pub(crate) fn terminal_target(&self) -> PathBuf {
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
                self.status = crate::i18n::tr_args(
                    cx,
                    "已在{name}中打开 {path}",
                    &[("name", name.clone()), ("path", dir.display().to_string())],
                );
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
        if self
            .quick_look
            .as_ref()
            .is_some_and(|look| look.path == path)
        {
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

    // ---- 压缩包 ----

    /// 光标（或唯一选中项）指着的是不是一个压缩包。
    fn archive_target(&self) -> Option<PathBuf> {
        let path = self
            .selection
            .cursor()
            .or_else(|| self.selection.single())?;
        archive::is_archive(path).then(|| path.to_path_buf())
    }

    /// 打开或关掉压缩包面板；已经在看同一个就关掉。
    fn toggle_archive(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.archive_target() else {
            self.close_archive(cx);
            return;
        };
        if self.archive_view.as_ref().is_some_and(|it| it.path == path) {
            self.close_archive(cx);
            return;
        }
        self.open_archive(path, cx);
    }

    /// 读一个压缩包的内容并把它摆在面板里。
    ///
    /// 读不出来也要开面板：把原因写进去，
    /// 比"按了没反应"或者"弹个框"都好交代。
    fn open_archive(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let (entries, error) = match archive::entries(&path) {
            Ok(entries) => (entries, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        self.archive_view = Some(ArchiveState {
            path,
            name,
            entries,
            error,
        });
        cx.notify();
    }

    fn close_archive(&mut self, cx: &mut Context<Self>) {
        if self.archive_view.take().is_some() {
            cx.notify();
        }
    }

    /// 打开或关掉搜索面板。搜索范围就是当前目录（含子目录）。
    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.close_search(window, cx);
            return;
        }
        let root = self.listing.path().to_path_buf();
        let panel = cx.new(|cx| SearchPanel::new(root, window, cx));
        // 面板自己不认识窗格：跳文件、关面板都靠事件回传。
        self.subscriptions.push(cx.subscribe_in(
            &panel,
            window,
            |this, _, event: &SearchEvent, window, cx| match event {
                SearchEvent::Close => this.close_search(window, cx),
                SearchEvent::Reveal(path) => this.reveal_path(path.clone(), window, cx),
            },
        ));
        // 面板这一帧才挂上窗口，焦点要等它挂上去再给输入框：
        // 新建实体时那个 focus 抢不到还没渲染的元素。
        let focus = panel.read(cx).input_handle(cx);
        window.focus(&focus, cx);
        self.search = Some(panel);
        cx.notify();
    }

    /// 有没有浮层压在窗格上。有浮层时窗格的命令一律不执行
    /// （见 [`Self::run_command`]），键盘也归浮层。
    fn overlay_open(&self) -> bool {
        self.search.is_some() || self.marking.is_some() || self.analyze.is_some()
    }

    /// 分析面板开没开。键盘与 Esc 的优先级要用它排顺序。
    fn analyze_open(&self) -> bool {
        self.analyze.is_some()
    }

    /// 打开分析面板，或者在开着的时候收起它。
    ///
    /// 面板里还能自己切页签（大小 / 重复 / 校验和），
    /// 所以这里只需要一条命令：按一下开，再按一下关。
    fn toggle_analyze(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.analyze.is_some() {
            self.close_analyze(window, cx);
            return;
        }
        let dir = self.listing.path().to_path_buf();
        let selected = self.analyze_targets();
        let mode = AnalyzeMode::default_for(&selected);
        let panel = cx.new(|cx| AnalyzePanel::new(dir, selected, mode, cx));
        self.subscriptions.push(cx.subscribe_in(
            &panel,
            window,
            |this, _, event: &AnalyzeEvent, window, cx| match event {
                AnalyzeEvent::Close => this.close_analyze(window, cx),
                AnalyzeEvent::Reveal(path) => this.reveal_path(path.clone(), window, cx),
            },
        ));
        // 面板这一帧才挂上窗口，焦点等挂好再给（见搜索面板那边的说明）。
        let focus = panel.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.analyze = Some(panel);
        cx.notify();
    }

    /// 关掉分析面板。实体一丢，`Drop` 会把后台那一趟中止掉。
    fn close_analyze(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.analyze.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// 分析算谁：选中项优先，一个都没选就用光标下那一项。
    ///
    /// 面板的三页各有各的口径（大小看目录、校验和看文件、重复看目录树），
    /// 但对"算谁"的回答必须一致，切页签不该换对象。
    fn analyze_targets(&self) -> Vec<PathBuf> {
        let selected = self.selected_paths();
        if !selected.is_empty() {
            return selected;
        }
        self.selection
            .cursor()
            .map(|path| vec![path.to_path_buf()])
            .unwrap_or_default()
    }

    /// 打开标记浮层：给选中的那一项标色、打标签。再按一次就收起。
    fn toggle_marks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selection.single().map(PathBuf::from) else {
            return;
        };
        if self.marking.as_ref().is_some_and(|it| it.path == path) {
            self.close_marks(window, cx);
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let current = Settings::read(cx)
            .marks
            .get(&path)
            .cloned()
            .unwrap_or_default();
        let joined = current.tags.join(" ");
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::tr(cx, "标签，空格分开"))
        });
        input.update(cx, |input, cx| input.set_value(&joined, window, cx));
        // Enter 由输入框自己吃掉（它把按键拦下了，冒泡收不到），
        // 所以这里订阅它的 PressEnter；Esc 走浮层的 `on_key_down`。
        self.subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_marks(window, cx);
                }
            },
        ));
        // 输入框是这一帧才挂上去的，焦点要等挂好再给（见搜索面板那边的说明）。
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.marking = Some(MarkState { path, name, input });
        cx.notify();
    }

    fn close_marks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.marking.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// 点了一个色块：立刻落盘，浮层不关——还要接着打标签。
    ///
    /// 再点同一个色块就是"把这个颜色去掉"。
    fn set_mark_color(&mut self, color: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.marking.as_ref() else {
            return;
        };
        let path = state.path.clone();
        let chosen = Settings::read(cx).marks.color_of(&path) == Some(color);
        Settings::update(cx, |settings| {
            settings
                .marks
                .set_color(&path, (!chosen).then(|| color.to_string()));
        });
        // 点色块会把焦点带走，点完还回输入框：用户还要接着打标签。
        let focus = state.input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 存下标签并收起浮层。
    fn save_marks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.marking.as_ref() else {
            return;
        };
        let path = state.path.clone();
        let tags = marks::parse_tags(state.input.read(cx).value().as_ref());
        Settings::update(cx, |settings| settings.marks.set_tags(&path, tags));
        self.close_marks(window, cx);
    }

    /// 颜色标签一起去掉。
    fn clear_marks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.marking.as_ref() else {
            return;
        };
        let path = state.path.clone();
        Settings::update(cx, |settings| settings.marks.clear(&path));
        self.close_marks(window, cx);
    }

    /// 跑一条存下来的搜索：先去它记的那个目录，再开面板把查询填进去。
    pub fn run_saved_search(
        &mut self,
        saved: &crate::settings::SavedSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.listing.path() != saved.root {
            self.navigate_to(saved.root.clone(), true, window, cx);
        }
        if self.search.is_none() {
            self.toggle_search(window, cx);
        }
        let Some(panel) = self.search.clone() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.set_query(&saved.query, window, cx));
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            // 面板一关，后台那趟搜索也该停：丢掉实体，`Cancel` 随之 drop。
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// 标记浮层：一排色块 + 一个标签输入框。
    fn render_marks(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.marking.clone() else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let current = Settings::read(cx)
            .marks
            .get(&state.path)
            .cloned()
            .unwrap_or_default();

        let mut card = v_flex()
            .id("marks-card")
            .test_support()
            .w(px(420.))
            .gap_3()
            .p_5()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(border)
            .bg(cx.theme().background)
            .child(div().font_semibold().truncate().child(state.name.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr(cx, "点一下标色，再点一下去掉")),
            )
            .child(
                h_flex().gap_2().children(
                    marks::COLORS
                        .iter()
                        .map(|(id, label)| {
                            let chosen = current.color.as_deref() == Some(*id);
                            div()
                                .id(format!("mark-color-{id}"))
                                .test_support()
                                .aria_label((*label).to_string())
                                .w(px(26.))
                                .h(px(26.))
                                .rounded_full()
                                .cursor_pointer()
                                .bg(rgb(marks::color_hex(id)))
                                .border_2()
                                .border_color(if chosen { foreground } else { border })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_mark_color(id, window, cx)
                                }))
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .child(Input::new(&state.input).w_full())
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("mark-clear")
                            .label(crate::i18n::tr(cx, "清除标记"))
                            .small()
                            .ghost()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear_marks(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("mark-save")
                            .label(crate::i18n::tr(cx, "完成"))
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_marks(window, cx)),
                            ),
                    ),
            );
        let used = Settings::read(cx).marks.all_tags();
        if !used.is_empty() {
            card = card.child(
                div()
                    .id("mark-used-tags")
                    .test_support()
                    .aria_label(used.join(" "))
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr_args(
                        cx,
                        "用过的标签：{tags}",
                        &[("tags", used.join(" "))],
                    )),
            );
        }

        div()
            .id("marks")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .on_click(cx.listener(|this, _, window, cx| this.close_marks(window, cx)))
            // 只吃 Esc 与 Enter：其余的键要放给输入框，
            // `stop_propagation` 连字符一起吞掉就打不进字了（见搜索面板）。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        this.close_marks(window, cx);
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.save_marks(window, cx);
                    }
                    _ => {}
                }
            }))
            .child(
                card.on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .into_any_element(),
            )
            .into_any_element()
    }

    /// 搜索结果里点了一个：切到它所在的目录并把它选中。
    /// 跳到某个文件：切到它所在的目录并选中它。搜索面板与分析、比较面板共用。
    pub fn reveal_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.close_search(window, cx);
        if path.is_dir() {
            self.navigate_to(path, true, window, cx);
            return;
        }
        let parent = path
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| path.clone());
        self.navigate_to(parent, true, window, cx);
        self.selection.select(path);
        self.note_restore();
        // 结果可能在列表很下面，把它滚回视野。
        if let Some(position) = self.selection.focus_position(&self.listing) {
            let row = self.row_of(position);
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        self.focus.focus(window, cx);
        self.update_status(cx);
        cx.notify();
    }

    /// 命令面板与 Esc 用：面板开着没有、这一项能不能看。
    pub fn archive_open(&self) -> bool {
        self.archive_view.is_some()
    }

    pub fn archive_enabled(&self) -> bool {
        self.archive_target().is_some()
    }

    /// 把面板里这个包解到当前目录下、以包命名的新文件夹里。
    ///
    /// 不直接摊在当前目录：一个装了两百个文件的包倒出来会把目录淹了。
    fn extract_here(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.archive_view.as_ref() else {
            return;
        };
        let zip = state.path.clone();
        // `报告.zip` → 解到 `报告/`；撞车就 `报告 2/`。
        let stem = zip
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| state.name.clone());
        let dest = archive::unused_dir(self.listing.path(), &stem);
        self.status = crate::i18n::tr_args(cx, "正在解压 {name}…", &[("name", state.name.clone())]);

        let (tx, rx) = async_channel::bounded(1);
        let wanted = Vec::new();
        // 解压是纯 IO + CPU 活，扔到普通线程；界面全程不碰它。
        std::thread::spawn(move || {
            let result = archive::extract(&zip, &dest, &wanted).map(|files| (dest, files));
            tx.try_send(result).ok();
        });
        self.run_archive_job(rx, cx);
    }

    /// 把选中的东西打成一个 zip，落在当前目录。
    fn create_archive(&mut self, cx: &mut Context<Self>) {
        let sources = self.selected_paths();
        if sources.is_empty() || self.archive_busy.is_some() {
            return;
        }
        let base = self.listing.path().to_path_buf();
        // 只选中一个就用它的名字；多个就用当前目录的名字。
        let stem = if sources.len() == 1 {
            sources[0]
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| crate::i18n::tr(cx, "归档"))
        } else {
            base.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| crate::i18n::tr(cx, "归档"))
        };
        let zip = archive::unused_name(&base, &stem, "zip");
        self.status = crate::i18n::tr_args(
            cx,
            "正在打包 {name}…",
            &[("name", zip.display().to_string())],
        );

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = archive::create(&zip, &base, &sources).map(|files| (zip, files));
            tx.try_send(result).ok();
        });
        self.run_archive_job(rx, cx);
    }

    /// 等一个后台的打包/解压活干完。
    ///
    /// 干活的线程只负责干，结论通过通道送回来再改界面状态。
    fn run_archive_job(
        &mut self,
        rx: async_channel::Receiver<Result<(PathBuf, usize), String>>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn(async move |this: WeakEntity<Pane>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            if let Ok(result) = rx.recv().await {
                this.update(&mut cx, |pane, cx| pane.finish_archive_job(result, cx))
                    .ok();
            }
        });
        self.archive_busy = Some(task);
        cx.notify();
    }

    fn finish_archive_job(
        &mut self,
        result: Result<(PathBuf, usize), String>,
        cx: &mut Context<Self>,
    ) {
        self.archive_busy = None;
        match result {
            Ok((target, files)) => {
                self.status = crate::i18n::tr_args(
                    cx,
                    "{target}：{count} 个文件",
                    &[
                        ("target", target.display().to_string()),
                        ("count", files.to_string()),
                    ],
                );
                self.refresh(cx);
            }
            Err(error) => {
                self.error = Some(error);
                self.update_status(cx);
            }
        }
        cx.notify();
    }

    /// Quick Look 开着没有。Esc 要先关它，再谈取消选中。
    pub fn quick_look_open(&self) -> bool {
        self.quick_look.is_some()
    }

    /// 当前 Quick Look 的状态，交给应用层去渲染。
    ///
    /// **为什么浮层画在应用层而不是窗格里**：Quick Look 是"看一眼整个文件"，
    /// 它不是某一栏的事。画在窗格里的话，遮罩只盖住侧栏右边的部分——
    /// 实测 1180px 的窗口里遮罩从 x=210 开始（侧栏右缘），标签栏和侧栏
    /// 都亮着，浮层被框在窗格中间；520px 的窗口里窗格只剩 310px 宽，
    /// 那张 720px 的卡片根本没法看。
    ///
    /// 状态留在窗格：Esc 的优先级、命令面板里"快速查看"那条显不显示，
    /// 都得知道**当前这个窗格**在看谁。
    pub fn quick_look_snapshot(&self) -> Option<QuickLookState> {
        self.quick_look.clone()
    }

    /// 关掉 Quick Look。应用层那个「关闭」按钮回调到这里。
    pub fn close_quick_look_from(&mut self, cx: &mut Context<Self>) {
        self.close_quick_look(cx);
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
        // 浮层（搜索面板、标记面板）开着的时候窗格被遮住了，
        // 命令不该在看不见的地方生效：那样在输入框里按 Cmd+C
        // 会一边复制文字、一边把选中文件塞进剪贴板。
        // 唯一例外是开关自己的那条——按它等于收起浮层。
        if self.overlay_open()
            && !matches!(
                command,
                PaneCommand::Search | PaneCommand::Mark | PaneCommand::Analyze
            )
        {
            return;
        }
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
            PaneCommand::Search => self.toggle_search(window, cx),
            PaneCommand::Mark => self.toggle_marks(window, cx),
            PaneCommand::ViewArchive => self.toggle_archive(cx),
            PaneCommand::CreateArchive => self.create_archive(cx),
            PaneCommand::ViewList => self.set_view(ViewMode::List, cx),
            PaneCommand::ViewGrid => self.set_view(ViewMode::Grid, cx),
            PaneCommand::ViewColumns => self.set_view(ViewMode::Columns, cx),
            PaneCommand::Analyze => self.toggle_analyze(window, cx),
            PaneCommand::EditRemote => self.edit_remote(cx),
        }
    }

    // ---- 目录监听与外部变更 ----

    /// 开始监听当前目录；切换目录时旧任务被丢弃，监听随之停止。
    fn start_watching(&mut self, cx: &mut Context<Self>) {
        // 远程目录没有本地文件事件可听：notify 只能看着本机的文件系统。
        if remote::parse_location(self.listing.path()).is_some() {
            self.watch_task = None;
            return;
        }
        self.watch_local(cx);
    }

    fn watch_local(&mut self, cx: &mut Context<Self>) {
        let path = self.listing.path().to_path_buf();
        let Ok((watcher, rx)) = watch::watch_directory(&path) else {
            return;
        };

        self.watch_task = None;
        // async move 前缀形式 + 克隆 AsyncApp：future 持有所有权，跨 await 安全。
        self.watch_task = Some(
            cx.spawn(async move |pane: WeakEntity<Pane>, cx: &mut AsyncApp| {
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
            }),
        );
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

    /// 顶部两行：**地址栏一行 + 工具栏一行**（跟 Win11 Explorer 一样）。
    ///
    /// 为什么不是挤在一行：地址栏是"我在哪"，工具栏是"我能做什么"，
    /// 两件事的信息密度完全不同。地址栏那一行里真正不可压缩的只有
    /// 3 颗导航 + 刷新 + 筛选框 = 328 px；工具栏那一行 4 颗操作 + 更多 +
    /// 3 颗视图 + 详情开关 = 216 px。**分开摆，两行都装得下**——
    /// 520 px 的窗口收掉侧栏之后还有 472 px，两行各自都富余。
    ///
    /// 挤在一行时是 908 px，比任何一档窗口都宽，于是我先砍按钮、再加
    /// 折叠开关、还给筛选框设上限——全是给这个错误结构打的补丁。
    /// 结构对了，那些补丁里有一半不必存在。
    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(self.render_address_bar(cx))
            .child(self.render_command_bar(cx))
    }

    /// 第一行：后退 / 前进 / 上一级 + 面包屑 + 刷新 + 筛选框。
    fn render_address_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("address-bar")
            .test_support()
            .h(px(40.))
            .px_3()
            .gap_2()
            .child(
                Button::new("back")
                    .icon(IconName::ChevronLeft)
                    .ghost()
                    .small()
                    .tooltip(crate::i18n::tr(cx, "后退"))
                    .disabled(self.back.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ChevronRight)
                    .ghost()
                    .small()
                    .tooltip(crate::i18n::tr(cx, "前进"))
                    .disabled(self.forward.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.go_forward(window, cx))),
            )
            .child(
                Button::new("up")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .small()
                    .tooltip(crate::i18n::tr(cx, "上一级"))
                    .disabled(self.listing.path().parent().is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
            )
            // 面包屑吃掉这一行剩下的全部宽度：窄的时候它先被压到 18px，
            // 再窄就只剩那条分隔线，但筛选框与刷新永远还在。
            .child(self.render_breadcrumb(cx))
            .child(
                Button::new("refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .small()
                    .tooltip(crate::i18n::tr(cx, "刷新"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            // 筛选框在这一行的**最右**：它是"看我眼前这些"，跟"我在哪"
            // 是两回事，挤在导航按钮旁边会让人以为它也是导航。
            //
            // 宽度写在槽上而不是 `Input` 上：`Input` 是固定宽度时它自己
            // 就是布局的硬底，槽再怎么缩也缩不动它。
            .child(
                div()
                    .id("filter-slot")
                    .test_support()
                    .min_w(px(96.))
                    .max_w(px(200.))
                    .flex_shrink(1.)
                    .child(
                        Input::new(&self.filter)
                            .prefix(Icon::new(IconName::Search).size_4())
                            .cleanable(true)
                            .min_w(px(96.))
                            .w_full(),
                    ),
            )
    }

    /// 第二行：文件操作 + 「更多」+ 视图切换 + 详情板开关。
    ///
    /// 这一行**不做任何横向压缩**：它最宽也只有 216 px，而地址栏行已经
    /// 把面包屑能压的都压掉了。不加 `overflow_x_scroll`——加了就是"这里
    /// 迟早会溢出"的自我承认，而实测它在任何窗口宽度下都不溢出。
    fn render_command_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("command-bar")
            .test_support()
            .h(px(40.))
            .px_3()
            .gap_2()
            .children(self.render_file_ops(cx))
            .child(self.render_more_menu(cx))
            .child(div().flex_1())
            .children(self.render_view_buttons(cx))
            .child(
                Button::new("toggle-details")
                    .icon(IconName::PanelRight)
                    .ghost()
                    .small()
                    .selected(self.details_shown)
                    .tooltip(if self.details_shown {
                        crate::i18n::tr(cx, "收起详情板（Cmd+Alt+O）")
                    } else {
                        crate::i18n::tr(cx, "展开详情板（Cmd+Alt+O）")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.details_shown = !this.details_shown;
                        cx.notify();
                    })),
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
                    .tooltip(crate::i18n::tr_args(
                        cx,
                        "{view}视图（{shortcut}）",
                        &[
                            ("view", crate::i18n::tr(cx, mode.label())),
                            ("shortcut", mode.shortcut().to_string()),
                        ],
                    ))
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

    /// 文件操作按钮组：**常显的四颗**。
    ///
    /// 留在外面的是**最高频**的：新建文件夹、拷贝、剪切、粘贴。粘贴紧跟
    /// 在剪切后面，两个连着按才是「移动」这个最常见的组合。
    ///
    /// 删除也高频，但它不可逆，位置太显眼反而危险——放进「更多」里，
    /// 那里和批量重命名、压缩包这些低频操作作伴。
    ///
    /// **注意理由不是「放不下」**：工具栏独占一行，216 px 就够摆十几颗，
    /// 哪儿都放得下。收进菜单是为了**少读一遍**——一排十二颗图标按钮里
    /// 十颗用户一年按不到两次，那些图标只是在稀释那四颗真正有用的。
    /// （`docs/ui-redesign.md` §0 定的也是「5 常显 + 1 更多」。）
    fn render_file_ops(&mut self, cx: &mut Context<Self>) -> Vec<Button> {
        let selected = self.selection.len();
        let can_write = self.error.is_none();
        let can_paste = !self.ops.read(cx).clipboard().paths.is_empty();

        vec![
            Button::new("op-new-folder")
                .icon(IconName::FolderPlus)
                .ghost()
                .small()
                .tooltip(crate::i18n::tr(cx, "新建文件夹（Cmd+Shift+N）"))
                .disabled(!can_write)
                .on_click(cx.listener(|this, _, _, cx| this.request_new_folder(cx))),
            Button::new("op-copy")
                .icon(IconName::Copy)
                .ghost()
                .small()
                .tooltip(crate::i18n::tr(cx, "拷贝（Cmd+C）"))
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.request_copy(cx))),
            Button::new("op-cut")
                .icon(IconName::Scissors)
                .ghost()
                .small()
                .tooltip(crate::i18n::tr(cx, "剪切（Cmd+X）"))
                .disabled(selected == 0)
                .on_click(cx.listener(|this, _, _, cx| this.request_cut(cx))),
            Button::new("op-paste")
                .icon(IconName::ClipboardPaste)
                .ghost()
                .small()
                .tooltip(crate::i18n::tr(cx, "粘贴（Cmd+V）"))
                .disabled(!can_paste)
                .on_click(cx.listener(|this, _, _, cx| this.request_paste(cx))),
        ]
    }

    /// 「更多」下拉：剩下那些低频但必要的文件操作。
    ///
    /// 收进来的依据是**使用频率**，不是重要性：重命名一年按不到几次，
    /// 而它占着工具栏上跟「拷贝」同等显眼的位置。搬进菜单不损失可达性——
    /// 每一条的命令面板条目和快捷键都还在（快捷键往往还是最快的入口）。
    ///
    /// 置灰状态跟着选中数走：进得去、看到「当前不能按」比干脆不显示有用。
    fn render_more_menu(&mut self, cx: &gpui_kit::App) -> impl IntoElement {
        let selected = self.selection.len();
        let quick_look = self.quick_look_target().is_some();
        let archive_is_zip = self.archive_target().is_some();
        let me = self.me.clone();

        Button::new("op-more")
            .icon(IconName::Ellipsis)
            .ghost()
            .small()
            .tooltip(crate::i18n::tr(cx, "更多操作"))
            // 下面这个闭包是 `Fn` 而不是 `FnOnce`——`dropdown_menu` 每帧
            // 渲染都会调它一次（菜单实体关掉再开还要重建）。所以 `Rc` 的
            // clone 必须发生在**闭包体内**：写在外面就成了「每次调用都要
            // move 一次同一个 `me`」，第一帧就编译不过（E0507）。
            .dropdown_menu(move |menu, _window, cx| {
                let me_rename = me.clone();
                let me_bulk = me.clone();
                let me_trash = me.clone();
                let me_copy_path = me.clone();
                let me_copy_name = me.clone();
                let me_look = me.clone();
                let me_term = me.clone();
                let me_archive = me.clone();

                menu.item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "重命名"))
                        .icon(IconName::SquarePen)
                        .disabled(selected != 1)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_rename, cx, |pane, cx| pane.request_rename(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "批量重命名"))
                        .icon(IconName::TextCursorInput)
                        .disabled(selected < 2)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_bulk, cx, |pane, cx| pane.request_bulk_rename(cx));
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "移到回收站"))
                        .icon(IconName::Trash)
                        .disabled(selected == 0)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_trash, cx, |pane, cx| pane.request_trash(cx));
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "复制路径"))
                        .icon(IconName::Link)
                        .disabled(selected == 0)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_copy_path, cx, |pane, cx| pane.copy_paths(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "复制文件名"))
                        .icon(IconName::Signature)
                        .disabled(selected == 0)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_copy_name, cx, |pane, cx| pane.copy_file_names(cx));
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "快速查看"))
                        .icon(IconName::Eye)
                        .disabled(!quick_look)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_look, cx, |pane, cx| pane.toggle_quick_look(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(crate::i18n::tr(cx, "在这里打开终端"))
                        .icon(IconName::Terminal)
                        .on_click(move |_, _, cx| {
                            run_on_pane(&me_term, cx, |pane, cx| pane.open_terminal(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(if archive_is_zip {
                        crate::i18n::tr(cx, "查看压缩包内容")
                    } else {
                        crate::i18n::tr(cx, "打包成 Zip")
                    })
                    .icon(IconName::FileArchive)
                    .disabled(selected == 0)
                    .on_click(move |_, _, cx| {
                        run_on_pane(&me_archive, cx, |pane, cx| {
                            if pane.archive_target().is_some() {
                                pane.toggle_archive(cx)
                            } else {
                                pane.create_archive(cx)
                            }
                        });
                    }),
                )
            })
    }

    /// 面包屑：每一段都能点，段数太多时把老段收进一个「…」。
    ///
    /// **为什么必须折叠而不是压缩**：路径层级是用户选的，深浅不由窗口宽窄决定。
    /// 一条 6 层深、名字都不短的路径，按最小宽度排开要 600 px 出头，而 520 px
    /// 窗口的面包屑容器只有 216 px。祖先段压到最小也装不下——最后只能让**当前
    /// 目录**跑到屏幕外去（实测它在 x=582，容器右边缘才 372）。而"我在哪"恰恰
    /// 是用户此刻最需要看见的一行。
    ///
    /// 折叠方案：超量的老段合并成一个「…」，点它回上一级——少看几层祖先，
    /// 一路点上去仍然能到根。当前目录那一段永远不折叠、名字太长出省略号。
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

        // 根目录那一段永远留着：它是"回到起点"的路，折叠掉就没有了。
        let last = crumbs.len().saturating_sub(1);
        let keep_from = last.saturating_sub(MAX_CRUMBS - 1);
        let folded = keep_from > 0;
        // 「…」点回去的目标：被折叠掉的最深那一段的父目录。
        let folded_target = if folded {
            crumbs[keep_from]
                .1
                .parent()
                .map(|parent| parent.to_path_buf())
        } else {
            None
        };

        let hover = cx.theme().muted;
        let current_color = cx.theme().foreground;
        let mut children: Vec<AnyElement> = Vec::new();

        if let Some(target) = folded_target {
            children.push(
                h_flex()
                    .id("crumb-folded")
                    .test_support()
                    .flex_shrink_0()
                    .child(
                        Button::new("crumb-more")
                            .ghost()
                            // 不给 `small()`：它带着的横向内边距在 216px 宽的
                            // 容器里就是 15px 的净支出，那是最贵的一段宽度。
                            // 只有一个字符，32px 的方块靠 tooltip 说明它是什么。
                            .label("…")
                            .tooltip(crate::i18n::tr(cx, "回到上一级"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate_to(target.clone(), true, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .child(Icon::new(IconName::ChevronRight).size_3()),
                    )
                    .into_any_element(),
            );
        }

        for (index, (label, target)) in crumbs.into_iter().enumerate().skip(keep_from) {
            let is_current = index == last;
            let crumb = div()
                .id(("crumb", index))
                // 布局测试要量每一段落在哪儿："老段收进「…」"这句话得靠量坐标
                // 才验得了——光看整条路径的宽度区分不出"折叠了"和"全挤扁了"
                // 这两种完全不同的结果。
                .test_support()
                .h(px(22.))
                .px_1()
                .flex()
                .items_center()
                .rounded(cx.theme().radius)
                .child(div().min_w_0().truncate().child(label));

            let crumb = if is_current {
                // 当前目录**也参与收缩**，但收缩因子比祖先段大得多
                // （8 比 1）：所以真正不够宽时它让得最少、留下最多。
                //
                // 之前它是 `flex_shrink_0` + `max_w(280)`，一个像素都不肯让：
                // 实测 520px 窗口里它右缘622，容器右缘才 372——用户最需要看的
                // "我在哪"整个挂在地址栏外面。而它保底还有 60 px，够认出一个
                // 目录名，不会缩成一条竖线。
                crumb
                    // 40px 保底是一个汉字加省略号——刚好能认出"这是哪个目录的
                    // 开头"，再窄就只剩一个「…」，那还不如把宽度让给祖先段。
                    .min_w(px(40.))
                    .flex_shrink(6.)
                    .max_w(px(280.))
                    .font_medium()
                    .text_color(current_color)
            } else {
                // 祖先段**不给 min_w**：那是最小尺寸的硬底，一有它整行就永远
                // 缩不进容器（实测给20px 保底时，520px 窗口里行宽仍是 236px，
                // 而容器只有 216px，`justify_end` 把整行推到右边越界 20px）。
                //
                // 压没了也点得到：祖先被压到看不见时，左边那个「…」连着上一级，
                // 一路点上去照样能到根。当前目录那一段有 40px 保底，用户始终
                // 看得见"我在哪"。
                //
                // 收缩因子 2 比祖先段当前的 1 高不了多少，但配合"祖先段没有
                // min_w"这一点就够：多出来的那点宽度差异全从祖先身上出，
                // 它们是那个可以整段消失的东西。
                crumb
                    .min_w_0()
                    .flex_shrink(2.)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_to(target.clone(), true, window, cx);
                    }))
            };

            children.push(
                h_flex()
                    .min_w_0()
                    // 这一层必须是可收缩的：段上那个 `flex_shrink(N)` 只有在父级
                    // 提供可分配空间时才起作用，外层 group 若是 `flex_shrink_0`，
                    // 它的宽度就由内容定死，段里的收缩因子形同虚设
                    // （实测祖先段停在 260 px 不动，整行因此宽过容器）。
                    .flex_shrink(1.)
                    .when(index > keep_from, |group| {
                        group.child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .child(Icon::new(IconName::ChevronRight).size_3()),
                        )
                    })
                    .child(crumb)
                    .into_any_element(),
            );
        }

        div()
            .id("breadcrumb")
            .test_support()
            .h(px(32.))
            .flex_1()
            .min_w_0()
            .px_2()
            .flex()
            .items_center()
            // 有 `justify_end` 才对：路径从右边起排是 Explorer 的习惯，
            // 而且超长的老段从**左边**被裁掉时，留在屏幕里的那些仍然贴右。
            // 行宽现在由折叠后的段数约束着，不会再宽过容器。
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
            // 图标占位 20px + 色点占位 8px：行里就是这么摆的，表头也得留出同一段，
            // 否则"名称"三个字比下面所有行的文件名右移 20 px。
            .child(div().w(px(20.)).flex_shrink_0())
            .child(div().w(px(8.)).flex_shrink_0())
            .child(self.header_cell(0, SortKey::Name, crate::i18n::tr(cx, "名称"), None, cx))
            .when_some(self.meta_label(cx), |this, label| {
                this.child(
                    div()
                        .w(px(COL_META))
                        .flex_shrink_0()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
            })
            .child(self.header_cell(
                1,
                SortKey::Size,
                crate::i18n::tr(cx, "大小"),
                Some(COL_SIZE),
                cx,
            ))
            .child(self.header_cell(
                2,
                SortKey::Modified,
                crate::i18n::tr(cx, "修改时间"),
                Some(COL_MODIFIED),
                cx,
            ))
            .child(self.header_cell(
                3,
                SortKey::Kind,
                crate::i18n::tr(cx, "类型"),
                Some(COL_KIND),
                cx,
            ))
    }

    /// `width` 为 `None` 时这一列吃掉剩余宽度（名称列）。
    fn header_cell(
        &self,
        index: usize,
        key: SortKey,
        label: String,
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

        let dragged = self.dragged_paths(&path);
        let drop_here = is_dir.then(|| path.clone());
        let item = ListItem::new(("row", position))
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
                    // 色点槽**整列常驻**，标了色的画真颜色，没标记的画透明的——
                    // 宽度一样、gap 一样，于是所有行的名字都从同一处起排。
                    //
                    // 原来只在标了色的时候才插这颗点，于是那一行的文字比旁边
                    // 所有行右移 8 px（点宽）加一个 12 px 的 gap——上下行对不齐，
                    // 整列名字看过去是波浪，而"这一行有没有标记"是它自己的事，
                    // 不该让整列的字都跟着错开 20 px。表头的「名称」列也就对上了。
                    .child({
                        let hex = Settings::read(cx)
                            .marks
                            .color_of(&path)
                            .map(marks::color_hex);
                        div()
                            .id(("row-mark", position))
                            .test_support()
                            .w(px(8.))
                            .h(px(8.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(match hex {
                                Some(hex) => rgb(hex),
                                // 全透明而不是 `transparent_white()`：后者是
                                // Hsla，而 rgb() 给的是 Rgba，两个类型不接。
                                None => rgba(0x00000000),
                            })
                    })
                    // 名字这一格靠外层那个 `row-name` 容器量：它在图标槽与色点槽
                    // **之前**，所以量它自己得不到"名字从哪儿起排"。要量起排位置
                    // 就量色点槽——它紧挨着名字，两者左边缘之差是固定的 12 px。
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .when_some(self.meta_text(&path, is_dir, cx), |this, text| {
                        // 整列常驻、里面可以空：不然每行少一个格子会左右跳。
                        this.child(
                            div()
                                .id(("row-meta", position))
                                .test_support()
                                .aria_label(text.clone())
                                .w(px(COL_META))
                                .flex_shrink_0()
                                .text_sm()
                                .truncate()
                                .text_color(muted)
                                .child(text),
                        )
                    })
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
                    this.update_status(cx);
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
            );
        // `ListItem` 只实现了 `IntoElement`，挂不上拖放那几个方法：
        // 外面套一层同高的 div 专管拖放，视觉上看不出多了一层。
        let row = div()
            .id(("row-drop", position))
            .test_support()
            .w_full()
            .h(px(ROW_HEIGHT))
            .child(item);
        let row = self.spring_load(row, drop_here.clone(), cx);
        self.with_drag_and_drop(row, Some(dragged), drop_here, cx)
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // 列表空白处也能接：落下来就进当前目录。
        let drop_here = self.listing.path().to_path_buf();
        if self.listing.visible_count() == 0 {
            let empty = self.render_empty(cx);
            return self.with_drag_and_drop(empty, None, Some(drop_here), cx);
        }

        let row_sizes = self.row_sizes.clone();
        let list = v_flex()
            .id("file-list")
            // 让集成测试能一眼看出"现在是列表还是网格"，顺带给读屏软件报一声。
            .test_support()
            .aria_label(crate::i18n::tr(cx, "列表视图"))
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
            .scrollbar(&self.scroll, ScrollbarAxis::Vertical);
        self.with_drag_and_drop(list, None, Some(drop_here), cx)
    }

    /// 空目录（或筛选后一个都不剩）时的居中提示，两种视图共用。
    fn render_empty(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let (icon, label) = if self.listing.query().is_empty() {
            (
                IconName::FolderOpen,
                crate::i18n::tr(cx, "这个文件夹是空的"),
            )
        } else {
            (IconName::SearchX, crate::i18n::tr(cx, "没有匹配的项目"))
        };
        div()
            // 带 id 才有 `Stateful<Div>`，拖放那几个方法挂在它上面。
            .id("empty-hint")
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
                    // 观察点与无障碍名放在**内层**这个 div 上，不放外层：
                    // 外层的返回类型是 `Stateful<Div>`（拖放方法挂在它上面），
                    // 而 `.test_support()` 会把它包成 `Observed<…>`，类型就变了。
                    // 文字本来就在这一层，放这里正好。
                    .id("empty-hint-label")
                    .test_support()
                    .aria_label(label.clone())
                    .text_sm()
                    .child(label),
            )
    }

    /// 网格：一个虚拟列表，但每一项是"一行格子"而不是一个文件。
    ///
    /// 之所以不直接用 `flex_wrap` 一把铺开：那样十万项会全部进布局树，
    /// 而 P2 的硬指标是十万项不卡。虚拟列表只画看得见的那几行。
    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // 网格空白处也能接：落下来就进当前目录。
        let drop_here = self.listing.path().to_path_buf();
        if self.listing.visible_count() == 0 {
            let empty = self.render_empty(cx);
            return self.with_drag_and_drop(empty, None, Some(drop_here), cx);
        }

        let row_sizes = self.row_sizes.clone();
        let columns = self.columns();
        let grid = v_flex()
            .id("file-grid")
            .test_support()
            .aria_label(crate::i18n::tr(cx, "网格视图"))
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
            .scrollbar(&self.scroll, ScrollbarAxis::Vertical);
        self.with_drag_and_drop(grid, None, Some(drop_here), cx)
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
            // 图注也是界面文案：英文用户看到的网格里也得写 Folder。
            crate::i18n::tr(cx, "文件夹")
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
            .child(div().text_xs().text_color(muted).truncate().child(caption));

        // 格子同样既是拖拽源也能接：拖到文件夹上就是放进去。
        let dragged = self.dragged_paths(&path);
        let drop_here = is_dir.then(|| path.clone());
        let tile = tile
            .when(is_selected, |tile| {
                tile.bg(cx.theme().muted)
                    .border_1()
                    .border_color(cx.theme().primary)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.select_row(path.clone(), event.modifiers);
                    this.update_status(cx);
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
            );
        let tile = self.spring_load(tile, drop_here.clone(), cx);
        Some(self.with_drag_and_drop(tile, Some(dragged), drop_here, cx))
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
            .aria_label(crate::i18n::tr(cx, "分列视图"))
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
    fn ancestor_column(&mut self, path: &Path, sort: Sort) -> ColumnContent {
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
        // 列的空白处也能接：落在哪一列的空白就进哪一层的目录。
        let drop_here = column.path.clone();

        let element = v_flex()
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
            );
        self.with_drag_and_drop(element, None, Some(drop_here), cx)
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
        // 拖起来的是哪些、能不能接：要在 `path` 被挪进鼠标闭包之前算好。
        let dragged = self.dragged_paths(&path);
        let drop_here = is_dir.then(|| path.clone());
        let icon_color = if is_dir {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };

        let element = div()
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
                div().flex_shrink_0().text_color(icon_color).child(
                    Icon::new(if is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .size_4(),
                ),
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
                            this.update_status(cx);
                            cx.notify();
                        }
                        return;
                    }
                    this.select_row(path.clone(), event.modifiers);
                    this.update_status(cx);
                    let plain_double_click = event.click_count >= 2
                        && !event.modifiers.shift
                        && !event.modifiers.secondary();
                    if plain_double_click {
                        this.open_path(path.clone(), window, cx);
                    }
                    cx.notify();
                }),
            );
        let element = self.spring_load(element, drop_here.clone(), cx);
        self.with_drag_and_drop(element, Some(dragged), drop_here, cx)
    }

    /// 拖起来的是哪些文件。
    ///
    /// 点在已选中的项上就是整批选中项；点在一个没选中的项上就是它自己——
    /// 按下鼠标会把它选中，而那时候这一帧早就画完了，
    /// 所以这里按"它此刻在不在选中里"判断，结果和按下之后的状态一致。
    fn dragged_paths(&self, path: &Path) -> DraggedPaths {
        let paths = if self.selection.contains(path) {
            self.selected_paths()
        } else {
            vec![path.to_path_buf()]
        };
        DraggedPaths { paths }
    }

    /// 给一个元素挂上拖放：`dragged` 非空则它可以被拖出去，
    /// `destination` 非空则它同时也能接。
    ///
    /// 只有文件夹能接：往一个文件上拖没有任何合理语义。
    /// 列表空白处能接（落进当前目录），所以两种角色要能分开给。
    fn with_drag_and_drop<E>(
        &self,
        mut element: E,
        dragged: Option<DraggedPaths>,
        destination: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        E: StatefulInteractiveElement + Styled + IntoElement + 'static,
    {
        // 这些方法都是吃掉自己、吐回一个新的同类型，所以每次要接回来。
        if let Some(dragged) = dragged {
            let label = drag::label(&dragged.paths, cx);
            element = element.on_drag(dragged, move |_, _, _, cx: &mut App| {
                cx.new(|_| DragPreview {
                    label: label.clone(),
                })
            });
            // 拖出窗口交给系统（比如拖到 Finder）：把手上的路径交出去，
            // 系统自己会去拷，我们不再插手。
            element = element.external_drag_payload::<DraggedPaths>(|drag, _, _| {
                Some(ExternalDragPayload::Files(FileDragPaths::new(
                    drag.paths.iter().map(|path| (path.clone(), path.is_dir())),
                )))
            });
        }

        if let Some(destination) = destination {
            let guard = destination.clone();
            element = element.can_drop(move |value, _, _| {
                dropped_paths(value).is_some_and(|paths| drag::can_drop(&guard, &paths))
            });
            let internal = destination.clone();
            element = element.on_drop(cx.listener(move |this, drag: &DraggedPaths, window, cx| {
                this.drop_paths(drag.paths.clone(), internal.clone(), false, window, cx);
            }));
            element =
                element.on_drop(cx.listener(move |this, drag: &ExternalPaths, window, cx| {
                    let paths = drag.0.iter().cloned().collect();
                    this.drop_paths(paths, destination.clone(), true, window, cx);
                }));
        }
        element.into_any_element()
    }

    /// 给文件夹挂上"弹簧"：拖动时鼠标在它上面停一会儿就自己进去。
    ///
    /// 想拖到三层深的目录里，不这么做就得先一层层点开、再回来拖。
    /// 挂在鼠标移动上而不是悬停上：拖动过程中照样派发移动事件，
    /// 而悬停事件跟拖动不是一条链路，靠它判断会在某些平台上漏掉。
    fn spring_load<E>(&self, element: E, path: Option<PathBuf>, cx: &mut Context<Self>) -> E
    where
        E: InteractiveElement,
    {
        let Some(path) = path else {
            // 不是文件夹（或本来就不接），没有"里面"可进。
            return element;
        };
        element.on_mouse_move(
            cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                if !drag::wants_to_expand(true, event.dragging() && cx.has_active_drag()) {
                    return;
                }
                this.schedule_spring_load(path.clone(), window, cx);
            }),
        )
    }

    /// 预约一次"弹开"；已经停在同一个文件夹上就不重新计时。
    fn schedule_spring_load(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .spring
            .as_ref()
            .is_some_and(|spring| spring.path == path)
        {
            // 还停在同一个上：让它继续走完原来那段时间，别每次移动都从头计时。
            return;
        }
        self.spring_token += 1;
        let token = self.spring_token;
        let open = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(drag::HOVER_EXPAND_AFTER)
                .await;
            let _ = this.update_in(cx, |pane, window, cx| {
                let still_hovering = pane.spring.as_ref().is_some_and(|it| it.token == token);
                pane.spring = None;
                // 中途挪走了（号对不上）或者早就松手了，这一票作废。
                if !still_hovering || !cx.has_active_drag() {
                    return;
                }
                pane.navigate_to(open.clone(), true, window, cx);
            });
        })
        .detach();
        self.spring = Some(SpringLoad { path, token });
    }

    /// 一批文件落到了这个目录里。
    fn drop_paths(
        &mut self,
        paths: Vec<PathBuf>,
        destination: PathBuf,
        from_outside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !drag::can_drop(&destination, &paths) {
            return;
        }
        // 修饰键要在落下的这一刻读：拖动过程中随时可以按放 Alt 改主意。
        let action = drag::drop_action(from_outside, window.modifiers().alt);
        let cut = action == drag::DropAction::Move;
        self.ops.update(cx, |ops, cx| {
            ops.drop_into(paths.clone(), destination.clone(), cut, cx)
        });
        self.status = crate::i18n::tr_args(
            cx,
            "{action} {count} 项到 {target}",
            &[
                ("action", crate::i18n::tr(cx, action.label())),
                ("count", paths.len().to_string()),
                ("target", destination.display().to_string()),
            ],
        );
        cx.notify();
    }

    /// 分析面板开着时 ↑↓ 归它：窗格自己的光标不该跟着动。
    fn analyze_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(panel) = self.analyze.clone() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.move_cursor(delta, cx));
    }

    /// 分析面板开着时 Enter 归它：跳到它指的那一行的文件。
    fn analyze_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.analyze.clone() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.reveal_cursor(cx));
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
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.analyze_open() {
                    this.analyze_cursor(-1, cx);
                    return;
                }
                this.move_cursor(-1, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.analyze_open() {
                    this.analyze_cursor(1, cx);
                    return;
                }
                this.move_cursor(1, false, cx)
            }))
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
            .on_action(cx.listener(|this, _: &Open, window, cx| {
                // 分析面板开着的时候 Enter 是"跳到它指的那个文件"，
                // 不是打开窗格光标下的东西。
                if this.analyze_open() {
                    this.analyze_reveal(cx);
                    return;
                }
                this.open_cursor(window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenParent, window, cx| {
                this.run_command(PaneCommand::GoUp, window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| this.go_back(window, cx)))
            .on_action(cx.listener(|this, _: &GoForward, window, cx| this.go_forward(window, cx)))
            .on_action(cx.listener(|this, _: &Refresh, window, cx| {
                this.run_command(PaneCommand::Refresh, window, cx)
            }))
            // Esc 有优先顺序：先看有没有浮层要关，没有才轮到清空选中。
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                // Esc 有优先顺序：先看有没有浮层要关，没有才轮到清空选中。
                if this.quick_look_open() {
                    this.close_quick_look(cx);
                } else if this.archive_open() {
                    this.close_archive(cx);
                } else if this.analyze_open() {
                    this.close_analyze(window, cx);
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
            .on_action(cx.listener(|this, _: &Search, window, cx| {
                this.run_command(PaneCommand::Search, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Mark, window, cx| {
                this.run_command(PaneCommand::Mark, window, cx)
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
            .on_action(cx.listener(|this, _: &ViewArchive, _, cx| this.toggle_archive(cx)))
            .on_action(cx.listener(|this, _: &CreateArchive, _, cx| this.create_archive(cx)))
            .on_action(cx.listener(|this, _: &Analyze, window, cx| {
                this.run_command(PaneCommand::Analyze, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EditRemote, window, cx| {
                this.run_command(PaneCommand::EditRemote, window, cx)
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
        // 多选态那两句走 `MultiSelectionSummary`（纯函数，要能被单测），
        // 所以语言在这里解析一次而不是让那两个函数收 `cx`。
        let locale = Settings::read(cx).locale();

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
                .child(
                    div()
                        .id("detail-name")
                        .test_support()
                        .aria_label(entry.name.clone())
                        .font_semibold()
                        .truncate()
                        .child(entry.name.clone()),
                )
                .child(detail_row(
                    "kind",
                    crate::i18n::tr(cx, "类型"),
                    entry.kind_label(),
                    cx,
                ))
                .child(detail_row(
                    "size",
                    crate::i18n::tr(cx, "大小"),
                    if entry.is_dir {
                        "—".to_string()
                    } else {
                        format::file_size(entry.size)
                    },
                    cx,
                ))
                .child(detail_row(
                    "modified",
                    crate::i18n::tr(cx, "修改"),
                    format::modified(entry.modified),
                    cx,
                ))
                // 云盘状态只对选中那一项判：判一次要问一次文件系统，
                // 列表里十万行不能每行问一遍。
                .when_some(
                    crate::cloud::describe(&entry.path, crate::cloud::detected()),
                    |this, (root, state)| {
                        this.child(detail_row(
                            "cloud",
                            crate::i18n::tr(cx, "云盘"),
                            format!("{} · {}", root.label(), state.label()),
                            cx,
                        ))
                    },
                )
                .when_some(
                    mark_text(Settings::read(cx).marks.get(entry.path.as_path())),
                    |this, text| {
                        this.child(detail_row("marks", crate::i18n::tr(cx, "标记"), text, cx))
                    },
                )
                .child(detail_row(
                    "location",
                    crate::i18n::tr(cx, "位置"),
                    entry.path.display().to_string(),
                    cx,
                ))
                .into_any_element(),
            None if self.selection.len() > 1 => {
                let selected = self.selection.ordered(&self.listing);
                let summary = MultiSelectionSummary::of(
                    selected.len(),
                    selected
                        .iter()
                        .filter_map(|path| self.listing.entry(path))
                        .map(|entry| entry.name.clone()),
                );

                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_semibold()
                            // 读屏软件该报"选了几个"，不然用户只听见一串文件名。
                            .id("detail-selection")
                            .test_support()
                            .aria_label(summary.heading(locale))
                            .child(summary.heading(locale)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .text_sm()
                            .children(
                                summary
                                    .names
                                    .iter()
                                    .map(|name| div().truncate().child(name.clone())),
                            )
                            .when_some(summary.more(locale), |this, line| {
                                this.child(
                                    div().text_color(cx.theme().muted_foreground).child(line),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::i18n::tr(cx, "批量操作会按列表顺序逐个应用。")),
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
                .child(
                    div()
                        .text_sm()
                        .child(crate::i18n::tr(cx, "选择一个项目查看详情")),
                )
                .into_any_element(),
        };

        v_flex()
            .id("details-panel")
            .test_support()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .p_4()
            .child(
                div()
                    .pb_4()
                    .font_semibold()
                    .child(crate::i18n::tr(cx, "详细信息")),
            )
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
            .child(div().flex_shrink_0().child(crate::i18n::tr_args(
                cx,
                "按{key}{order}",
                &[
                    ("key", sort_label(sort, cx)),
                    // 升序/降序整个词都要查表，所以先查好再当实参传进去，
                    // 不在实参位置上再查一次。
                    (
                        "order",
                        crate::i18n::tr(cx, if sort.descending { "降序" } else { "升序" }),
                    ),
                ],
            )))
    }
}

impl Pane {
    // ---- 快捷键 ----

    /// 开始给一条命令录新按键。
    ///
    /// 焦点要挪到浮层上：GPUI 的按键事件只送到"焦点所在的那条链"，
    /// 浮层不在焦点链上就一个键也收不到。
    pub fn start_capture(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capturing = Some(command);
        self.capture_focus.focus(window, cx);
        cx.notify();
    }

    /// 录到了一个键。
    ///
    /// 退格/删除是"恢复出厂"——空着的命令没法按出来，得有路可退。
    /// Esc 是取消。其余的按键就当新键记下来。
    fn capture_key(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.capturing.take() else {
            return;
        };
        let mut map = keymap::KeyMap::read(cx);
        if matches!(key.as_str(), "backspace" | "delete") {
            map.clear(command);
            self.status = crate::i18n::tr_args(
                cx,
                "{title} 已恢复出厂按键",
                &[("title", Self::command_title(command, cx))],
            );
        } else {
            map.set(command, key.clone());
            // 带名字的占位符：英文是 "{title} is now {key}，takes effect after restart"，
            // 实参位置跟中文不同，位置占位符换不了。
            self.status = crate::i18n::tr_args(
                cx,
                "{title} 改为 {key}，重启后生效",
                &[("title", Self::command_title(command, cx)), ("key", key)],
            );
        }
        map.save(cx);
        // 浮层一撤，焦点就没了着落；不还回列表，Cmd+P 这类带上下文的键会失灵。
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 收起录按键的浮层（按 Esc 或录完都走这里）。
    fn end_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.capturing = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 窗格内部用的**短标题**（状态栏、按键浮层）。
    ///
    /// **故意比命令面板那套短**："拷贝"而不是"拷贝选中项"、"刷新"而不是
    /// "刷新当前目录"。窗格工具条上每一格只有几十像素宽，长标题要么被截断、
    /// 要么把整条工具条挤宽；而面板是纵向列表，宽裕，标题说全更清楚。
    /// 两套文案各自翻，不要试图合成一套——合成的结果是窗格里也变长。
    fn command_title(command: PaneCommand, cx: &gpui_kit::App) -> String {
        let text = match command {
            PaneCommand::NewFolder => "新建文件夹",
            PaneCommand::Rename => "重命名",
            PaneCommand::BulkRename => "批量重命名",
            PaneCommand::Trash => "移到回收站",
            PaneCommand::Copy => "拷贝",
            PaneCommand::Cut => "剪切",
            PaneCommand::Paste => "粘贴",
            PaneCommand::Undo => "撤销",
            PaneCommand::Refresh => "刷新",
            PaneCommand::GoUp => "上一级",
            PaneCommand::CopyPath => "复制路径",
            PaneCommand::CopyFileName => "复制文件名",
            PaneCommand::OpenTerminal => "打开终端",
            PaneCommand::QuickLook => "快速查看",
            PaneCommand::Search => "搜索当前目录",
            PaneCommand::Mark => "加标签 / 标颜色",
            PaneCommand::ViewArchive => "查看压缩包",
            PaneCommand::CreateArchive => "打包成 Zip",
            PaneCommand::ViewList => "列表视图",
            PaneCommand::ViewGrid => "网格视图",
            PaneCommand::ViewColumns => "分列视图",
            PaneCommand::Analyze => "分析：大小 / 重复 / 校验和",
            PaneCommand::EditRemote => "编辑远程文件（保存即回传）",
        };
        crate::i18n::tr(cx, text)
    }

    /// 录按键的浮层：整个窗格盖住，等你按一下。
    fn render_key_capture(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(command) = self.capturing else {
            return div().into_any_element();
        };
        let title = Self::command_title(command, cx);
        let current = keymap::KeyMap::read(cx).key_for(command);
        let muted = cx.theme().muted_foreground;

        div()
            .id("key-capture")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .track_focus(&self.capture_focus)
            // 捕获阶段就收下：默认的按键绑定（比如 Esc 清选中）不该趁机执行。
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                // 捕获阶段就收下：默认绑定（比如 Esc 清选中）不该趁机执行。
                cx.stop_propagation();
                if event.keystroke.key == "escape" {
                    this.end_capture(window, cx);
                    return;
                }
                let key = event.keystroke.unparse();
                if keymap::is_usable(&key) {
                    this.capture_key(key, window, cx);
                }
            }))
            .child(
                v_flex()
                    .id("key-capture-card")
                    .w(px(420.))
                    .gap_2()
                    .p_5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(div().font_semibold().child(crate::i18n::tr_args(
                        cx,
                        "给「{title}」按个键",
                        &[("title", title.clone())],
                    )))
                    .child(
                        div()
                            .id("key-capture-current")
                            .test_support()
                            .aria_label(current.clone())
                            .text_sm()
                            .text_color(muted)
                            .child(if current.is_empty() {
                                crate::i18n::tr(cx, "现在是：没有按键")
                            } else {
                                crate::i18n::tr_args(
                                    cx,
                                    "现在是：{current}",
                                    &[("current", current.clone())],
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(crate::i18n::tr(cx, "退格 = 恢复出厂，Esc = 取消")),
                    ),
            )
            .into_any_element()
    }

    /// 压缩包面板：列出包里的条目，配一个"解压到这儿"。
    fn render_archive(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.archive_view.clone() else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let busy = self.archive_busy.is_some();

        let files = state.entries.iter().filter(|entry| !entry.is_dir).count();
        let total: u64 = state.entries.iter().map(|entry| entry.size).sum();
        let subtitle = crate::i18n::tr_args(
            cx,
            "{files} 个文件 · 共 {total}",
            &[
                ("files", files.to_string()),
                ("total", format::file_size(total)),
            ],
        );

        let body = match &state.error {
            Some(error) => div()
                .id("archive-error")
                .test_support()
                .aria_label(error.clone())
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(error.clone())
                .into_any_element(),
            None if state.entries.is_empty() => div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(crate::i18n::tr(cx, "这个包是空的"))
                .into_any_element(),
            None => div()
                .id("archive-entries")
                .test_support()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .font_family(cx.theme().mono_font_family.clone())
                .text_sm()
                .child(
                    v_flex().children(
                        state
                            .entries
                            .iter()
                            .enumerate()
                            .map(|(index, entry)| self.render_archive_entry(index, entry, cx)),
                    ),
                )
                .into_any_element(),
        };

        let stem = state
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| state.name.clone());

        div()
            .id("archive")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .child(
                v_flex()
                    .id("archive-card")
                    .w(px(560.))
                    .max_h(px(560.))
                    .gap_3()
                    .p_5()
                    .rounded(radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(
                        v_flex()
                            .gap_1()
                            .min_w_0()
                            .child(div().font_semibold().truncate().child(state.name.clone()))
                            .child(
                                div()
                                    .id("archive-subtitle")
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
                            .gap_2()
                            .child(
                                Button::new("archive-extract")
                                    .label(crate::i18n::tr_args(
                                        cx,
                                        "解压到「{stem}」",
                                        &[("stem", stem.clone())],
                                    ))
                                    .small()
                                    .disabled(state.error.is_some() || busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.extract_here(cx))),
                            )
                            .child(
                                Button::new("archive-close")
                                    .label(crate::i18n::tr(cx, "关闭（Esc）"))
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| this.close_archive(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 面板里的一行：包名里的路径 + 大小。
    fn render_archive_entry(
        &self,
        index: usize,
        entry: &archive::Entry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if entry.is_dir {
            format!("{}/", entry.name.trim_end_matches('/'))
        } else {
            entry.name.clone()
        };
        div()
            .id(("archive-entry", index))
            .test_support()
            .aria_label(label.clone())
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded(cx.theme().radius)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(if entry.is_dir {
                        cx.theme().primary
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(
                        Icon::new(if entry.is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .size_4(),
                    ),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if entry.is_dir {
                        String::new()
                    } else {
                        format::file_size(entry.size)
                    }),
            )
            .into_any_element()
    }

    /// 元数据列的表头：显示的是"当前这一列是什么"。没开就是 `None`（整列不画）。
    fn meta_label(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.meta
            .update(cx, |meta, cx| meta.field(cx))
            .map(|field| field.label(cx))
    }

    /// 这一行元数据那一格写什么。`None` = 这一列现在不显示。
    fn meta_text(&mut self, path: &Path, is_dir: bool, cx: &mut Context<Self>) -> Option<String> {
        self.meta.update(cx, |meta, cx| meta.text(path, is_dir, cx))
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

/// 从一次投放里取出被拖的路径：可能是窗格里拖出来的，也可能是系统拖进来的。
pub(crate) fn dropped_paths(value: &dyn Any) -> Option<Vec<PathBuf>> {
    if let Some(drag) = value.downcast_ref::<DraggedPaths>() {
        return Some(drag.paths.clone());
    }
    value
        .downcast_ref::<ExternalPaths>()
        .map(|paths| paths.0.iter().cloned().collect())
}

/// 每种类型用什么图标。没有专门图标的就退回通用文件图标。
pub(crate) fn icon_for_kind(kind: preview::Kind) -> IconName {
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
            );
        // 详情板收起来时**整块不画**，而不是画成 0 宽：留着一条
        // 0 宽的分隔线和一个看不见的边框，看起来像"界面坏了"。
        if self.details_shown {
            root = root.child(self.render_details(cx));
        }

        if self.archive_view.is_some() {
            root = root.child(self.render_archive(cx));
        }
        if self.capturing.is_some() {
            root = root.child(self.render_key_capture(cx));
        }
        if self.marking.is_some() {
            root = root.child(self.render_marks(cx));
        }
        // 分析面板也自带遮罩，压在标记浮层上面。
        if let Some(panel) = self.analyze.clone() {
            root = root.child(panel.into_any_element());
        }
        // 搜索面板压在最上面：它自己带遮罩，且要盖住另外几个浮层。
        if let Some(panel) = self.search.clone() {
            root = root.child(panel.into_any_element());
        }
        root
    }
}

/// 状态栏里"现在按哪一列排"的标签（"名称 升序"那半句的前半截）。
fn sort_label(sort: Sort, cx: &gpui_kit::App) -> String {
    let text = match sort.key {
        SortKey::Name => "名称",
        SortKey::Size => "大小",
        SortKey::Modified => "修改时间",
        SortKey::Kind => "类型",
    };
    crate::i18n::tr(cx, text)
}

/// 详情面板里"标记"那一行：颜色加标签，两样都没有就不显示。
fn mark_text(mark: Option<&crate::marks::Mark>) -> Option<String> {
    let mark = mark?;
    if mark.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(color) = &mark.color {
        parts.push(marks::color_label(color).to_string());
    }
    parts.extend(mark.tags.iter().cloned());
    Some(parts.join(" · "))
}

/// 详情板多选态的文案。
///
/// **抽成纯函数是因为它测不到**：多选要 shift 或 Ctrl 点行，而 gpui-kit
/// 的测试 `mouse_down` 把 `modifiers` 写死成 `Default::default()`——
/// 造不出带修饰键的那次点击（`window.click` 走的正是它）。缝在这里而不是
/// 留在渲染函数里，是为了"选 12 个时标题写几个、尾巴写几个"这类性质
/// 能在单测里验；渲染树里那部分仍然是集成测试的地盘。
pub struct MultiSelectionSummary {
    /// 列出来的名字，至多 [`Self::MAX_NAMES`] 个。
    pub names: Vec<String>,
    /// 总共选了几个。
    pub total: usize,
}

impl MultiSelectionSummary {
    /// 详情板最多列几个名字，剩下的合成一句"还有 N 项"。
    ///
    /// 板子就那么宽，列五十个名字会把别的信息顶出去。
    pub const MAX_NAMES: usize = 8;

    pub fn of(total: usize, names: impl Iterator<Item = String>) -> Self {
        Self {
            names: names.take(Self::MAX_NAMES).collect(),
            // 名字可能比 `total` 少（`listing` 里查不到的那几个路径），
            // 那时总数跟着名字走：报一个比列出来的还大的数，
            // 用户数了数屏幕上的名字，发现对不上。
            total,
        }
    }

    /// 标题："已选中 N 项"。
    ///
    /// **按 `locale` 查表而不是收 `cx`**：这个函数存在的理由就是能被单测
    /// （上面那条注释说为什么渲染树里测不到），收 `cx` 就把单测的可能性
    /// 又拿走了。渲染那边传 `Settings::read(cx).locale()` 进来。
    pub fn heading(&self, locale: crate::i18n::Locale) -> String {
        crate::i18n::tr_args_in(
            locale,
            "已选中 {count} 项",
            &[("count", self.total.to_string())],
        )
    }

    /// 没列完时的那句尾巴；全列完了就是 `None`（不渲染这一行）。
    pub fn more(&self, locale: crate::i18n::Locale) -> Option<String> {
        let rest = self.total.checked_sub(self.names.len())?;
        (rest > 0).then(|| {
            crate::i18n::tr_args_in(locale, "…… 还有 {rest} 项", &[("rest", rest.to_string())])
        })
    }
}

/// 详情面板里的一行：左边标签（类型 / 大小 / 修改…），右边值。
///
/// `label` 收 `String` 而不是 `&str`：它已经查过表了（`tr()` 返回 `String`），
/// 让调用方再从 `&str` 转一次是白转。
///
/// **`key` 是稳定的英文标识，与 `label` 分开**：元素 id 用来让测试和界面调试
/// 定位"哪一行"，不能跟着界面语言变——否则切到英文之后 `detail-云盘`
/// 就找不到了，而那行内容一个字都没变。
fn detail_row(key: &'static str, label: String, value: String, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(
            div()
                // 每一行都挂 id：界面调试和集成测试按"哪一行"定位，不用数第几个。
                .id(format!("detail-{key}"))
                .test_support()
                .aria_label(value.clone())
                .text_sm()
                .truncate()
                .child(value),
        )
}

#[cfg(test)]
mod tests {
    use crate::model::{FileEntry, Listing, Selection};
    use crate::pane::{MultiSelectionSummary, Pane};
    use std::path::PathBuf;

    /// 一个装着远程文件的列表：路径按 `remote://连接 id/服务器上路径` 拼。
    ///
    /// 不开窗口也能造——`editable_remote` 只看选中项和列表，不看界面。
    fn remote_listing() -> (Listing, PathBuf, PathBuf) {
        let dir = crate::remote::location("t1", "folder");
        let file = crate::remote::location("t1", "folder/notes.txt");
        let sub = crate::remote::location("t1", "folder/sub");
        let listing = Listing::from_entries(
            dir.clone(),
            vec![
                FileEntry::remote(file.clone(), "notes.txt", false, 1234, None),
                FileEntry::remote(sub, "sub", true, 0, None),
            ],
        );
        (listing, dir, file)
    }

    #[test]
    fn a_single_remote_file_is_editable() {
        let (listing, _, file) = remote_listing();
        let mut selection = Selection::default();
        selection.select(file);
        assert_eq!(
            Pane::editable_remote(&selection, &listing),
            Some(("t1".to_string(), "folder/notes.txt".to_string())),
            "选中一个远程文件就该能编辑，且回传路径要对"
        );
    }

    #[test]
    fn a_directory_is_not_editable() {
        let (listing, _, _) = remote_listing();
        let mut selection = Selection::default();
        selection.select(crate::remote::location("t1", "folder/sub"));
        assert_eq!(
            Pane::editable_remote(&selection, &listing),
            None,
            "目录没有可编辑的内容"
        );
    }

    #[test]
    fn a_local_file_is_not_editable() {
        let (listing, _, _) = remote_listing();
        let mut selection = Selection::default();
        selection.select(PathBuf::from("/Users/me/notes.txt"));
        assert_eq!(
            Pane::editable_remote(&selection, &listing),
            None,
            "本地文件走系统编辑器，不归这条命令管"
        );
    }

    #[test]
    fn two_selected_files_are_not_editable() {
        let (listing, _, file) = remote_listing();
        let also = crate::remote::location("t1", "folder/sub");
        let mut selection = Selection::default();
        selection.select(file);
        selection.toggle(also);
        assert_eq!(
            Pane::editable_remote(&selection, &listing),
            None,
            "多选时说不清该编辑哪一个"
        );
    }

    #[test]
    fn nothing_selected_is_not_editable() {
        let (listing, _, _) = remote_listing();
        let selection = Selection::default();
        assert_eq!(Pane::editable_remote(&selection, &listing), None);
    }

    // ---- 详情板多选态的文案 ----
    //
    // 渲染树里测不到这一段（gpui-kit 的测试 `mouse_down` 写死
    // `modifiers: Default::default()`，造不出 shift 多选），所以把它
    // 缝成纯函数在这里验。

    fn names(count: usize) -> impl Iterator<Item = String> {
        (1..=count)
            .map(|n| format!("文件{n}"))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_selection_heading_counts_everything_even_past_the_listing_limit() {
        let summary = MultiSelectionSummary::of(12, names(12));
        assert_eq!(summary.names.len(), MultiSelectionSummary::MAX_NAMES);
        assert_eq!(summary.heading(crate::i18n::Locale::ZhHans), "已选中 12 项");
        // 12 个里只列了 8 个，尾巴该报 4 个，不是 12 个。
        assert_eq!(
            summary.more(crate::i18n::Locale::ZhHans),
            Some("…… 还有 4 项".to_string())
        );
    }

    #[test]
    fn a_selection_that_fits_has_no_trailing_line() {
        let summary = MultiSelectionSummary::of(3, names(3));
        assert_eq!(summary.heading(crate::i18n::Locale::ZhHans), "已选中 3 项");
        assert_eq!(summary.more(crate::i18n::Locale::ZhHans), None);
    }

    #[test]
    fn the_selection_heading_follows_the_language() {
        let summary = MultiSelectionSummary::of(12, names(12));
        let heading = summary.heading(crate::i18n::Locale::En);
        assert!(
            !heading.chars().any(|c| c as u32 > 0x2E80),
            "英文标题里还有中文：{heading:?}"
        );
        assert!(heading.contains("12"), "英文标题该带总数：{heading:?}");
        let rest = summary.more(crate::i18n::Locale::En).expect("该有尾巴");
        assert!(
            !rest.chars().any(|c| c as u32 > 0x2E80),
            "英文尾巴里还有中文：{rest:?}"
        );
        assert!(rest.contains('4'), "英文尾巴该报 4：{rest:?}");
    }

    #[test]
    fn names_that_are_missing_from_the_listing_do_not_break_the_count() {
        // 选了 5 个但只有 2 个还查得到（别的被别处删了）：
        // 标题照实说 5，尾巴说 3——报 2 会跟"选了几个"对不上。
        let summary = MultiSelectionSummary::of(5, names(2));
        assert_eq!(summary.heading(crate::i18n::Locale::ZhHans), "已选中 5 项");
        assert_eq!(
            summary.more(crate::i18n::Locale::ZhHans),
            Some("…… 还有 3 项".to_string())
        );
    }
}
