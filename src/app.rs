//! 应用根视图：标签栏、当前浏览窗格、文件操作对话框与底部状态栏。
//!
//! 标签页与会话恢复在这一层：每个标签是一个独立的 `Entity<Pane>`，
//! 打开的目录列表在变化时写盘，下次启动原样恢复。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, Sizable,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    *,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::bulk_rename::BulkRename;
use crate::cli;
use crate::clipboard::name_of;
use crate::compare::Options as CompareOptions;
use crate::compare_entity::{CompareEvent, ComparePanel};
use crate::diff_entity::{DiffEvent, DiffPanel};
use crate::drag::{self, DraggedPaths};
use crate::format;
use crate::instance;
use crate::meta::{Field as MetaField, Metadata};
use crate::ops::{ConflictPolicy, ControlState};
use crate::ops_entity::{Ops, OpsEvent, PendingAction};
use crate::palette::{self, Command as PaletteCommand};
use crate::pane::{Pane, PaneCommand, PaneEvent, QuickLookState};
use crate::preview::Preview;
use crate::remote::{self, Kind as RemoteKind};
use crate::remotes::{Remotes, RemotesEvent};
use crate::session::Session;
use crate::settings::{Layout, Settings};
use crate::term_entity::{TOGGLE_KEY, Terminal, TerminalEvent};
use crate::terminal;
use crate::thumbnails::Thumbnails;

/// 侧栏展开时的宽度。
///
/// 210 px 是量出来的下限，不是随手取的整数：里面要放下「位置」四条的
/// 图标（16）+ 间隙（12）+ 文字（最长"个人文件夹"5 字 ≈ 70 px）+ 内边距，
/// 少于 210 文字就得截断。
const SIDEBAR_WIDTH: f32 = 210.;
/// 侧栏收起时那条图标条的宽度：内边距 8 + 按钮 32。
const SIDEBAR_COLLAPSED_WIDTH: f32 = 48.;

gpui_kit::actions!(
    wfiles,
    [
        NewTab,
        CloseTab,
        NextTab,
        TogglePalette,
        ToggleSplit,
        SwapPane,
        ToggleSync,
        CopyToOther,
        MoveToOther,
        ToggleTerminal,
        ToggleSidebar,
        ToggleDetails,
        Compare,
        Diff,
    ]
);

/// 批量重命名对话框里的五个输入框，随开随建、随关随毁。
#[derive(Clone)]
struct BulkInputs {
    template: Entity<InputState>,
    find: Entity<InputState>,
    replace: Entity<InputState>,
    start: Entity<InputState>,
    step: Entity<InputState>,
}

/// 命令面板一条候选项：展示信息加上要执行的动作。
#[derive(Clone)]
struct PaletteItem {
    command: PaletteCommand,
    action: PaletteAction,
}

/// 应用级命令：不属于任何一个窗格（分栏、同步这类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppCommand {
    ToggleSplit,
    SwapPane,
    ToggleSync,
    CopyToOther,
    MoveToOther,
    /// 收起 / 展开侧栏。
    ToggleSidebar,
    /// 显示 / 隐藏详情板。
    ToggleDetails,
    /// 比较两栏（或选中的两个目录）差在哪，并可选同步。
    Compare,
    /// 并排看两个文件的内容差在哪。
    Diff,
}

/// 面板能触发的动作。窗格内的操作转交给当前窗格，
/// 应用级的（标签页）由应用自己处理。
#[derive(Clone)]
enum PaletteAction {
    /// 分栏、同步这类应用级命令。
    App(AppCommand),
    GoTo(PathBuf),
    SwitchTab(usize),
    Pane(PaneCommand),
    /// 改某条命令的快捷键；会打开按键捕获浮层。
    EditKey(PaneCommand),
    /// 把某个终端设为默认；只改设置，不打开窗口。
    SetTerminal(String),
    /// 列表右边那一列元数据显示什么；`None` = 不显示这一列。
    MetaColumn(Option<MetaField>),
    /// 开关底部的内嵌终端面板。
    ToggleTerminal,
    NewTab,
    CloseTab,
    NextTab,
    /// 跑一条存下来的搜索：打开搜索面板并把那句查询填进去。
    SavedSearch(String),
    /// 打开“连接远程服务器”对话框。
    Connect,
    /// 打开一条存下来的连接（存的是连接 id）。
    OpenRemote(String),
    /// 设一个搬运限速（字节每秒，0 = 不限速）。
    SetSpeedLimit(u64),
    /// 换界面语言。
    SetLocale(crate::i18n::Locale),
    /// 界面语言改回"跟随系统"（设置里存空串）。
    SetLocaleAuto,
    /// 把日志导出到桌面（缓存目录里那份用户自己找不到）。
    ExportLog,
}

/// 连接对话框的当前状态。输入框随开随建、随关随毁，密码不留在别处。
#[derive(Clone)]
struct ConnectState {
    /// 选了哪种协议。不同协议要填的栏位不一样（S3 多一个地区）。
    kind: RemoteKind,
    name: Entity<InputState>,
    url: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// 地区。只有 S3 用得上，其余协议这一栏不显示。
    region: Entity<InputState>,
    /// 私钥文件的路径。只有 SFTP 用得上，其余协议这一栏不显示。
    key: Entity<InputState>,
    /// 正在后台试连：按钮变灰，也不再重复发起。
    busy: bool,
    /// 上一次试连失败的原因。
    error: Option<crate::i18n::Msg>,
}

/// 命令面板的当前状态。候选项在打开时快照，
/// 这样面板开着的时候改不到列表，行为可预测。
struct PaletteState {
    items: Vec<PaletteItem>,
    /// 过滤后的下标，按相关度排序。
    filtered: Vec<usize>,
    selected: usize,
}

/// 从服务器地址里取个主机名出来当默认显示名：
/// `https://dav.jianguoyun.com/dav/` → `dav.jianguoyun.com`。
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', ':']).next().unwrap_or_default();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// 协议选择按钮的 id：测试要能点它，就给它一个稳的名字。
fn kind_id(kind: RemoteKind) -> String {
    match kind {
        RemoteKind::WebDav => "connect-kind-webdav".to_string(),
        RemoteKind::S3 => "connect-kind-s3".to_string(),
        RemoteKind::Sftp => "connect-kind-sftp".to_string(),
        RemoteKind::Ftp => "connect-kind-ftp".to_string(),
    }
}

/// 应用级快捷键：不依赖任何上下文，任何地方按下都生效。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("cmd-p", TogglePalette, None),
        // 双栏：Cmd+Alt+D 分栏、Tab 换焦点。Tab 是 Total Commander 那一系
        // 双栏管理器的通用键位，单栏时按了没反应。
        KeyBinding::new("cmd-alt-d", ToggleSplit, None),
        KeyBinding::new("tab", SwapPane, None),
        KeyBinding::new("cmd-alt-s", ToggleSync, None),
        // F5/F6 拷到/移到另一栏，同样是双栏管理器沿用多年的键位。
        KeyBinding::new("f5", CopyToOther, None),
        KeyBinding::new("f6", MoveToOther, None),
        // 内嵌终端面板。Ctrl+Alt+T 是 Linux 桌面上"打开终端"的老键位，
        // 换成 Ctrl 也和"打开系统终端"的 Cmd+Alt+T 区分得开。
        KeyBinding::new(TOGGLE_KEY, ToggleTerminal, None),
        // 侧栏 / 详情板显隐。Cmd+B 是编辑器与文件管理器的老键位
        // （VS Code 的侧栏、Finder 的检查器都占它）。
        //
        // 详情板用 Cmd+Alt+O：Cmd+Alt+I 已经是「分析面板」、
        // Cmd+Alt+E 是「编辑远程文件」。两个功能都是"换个面板看看"，
        // 抢键会让其中一个永远按不出来，而且谁都不会去查另一处绑在哪。
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-alt-o", ToggleDetails, None),
        // 比较：双栏时比两栏，单栏时比选中的两个目录。
        KeyBinding::new("cmd-alt-b", Compare, None),
        // 文件内容比较：同上，只是对象换成两个文件。带 Shift 是顺着
        // "同一个比较动作、更细一层"的意思，和上面的目录比较挨着记。
        KeyBinding::new("cmd-alt-shift-b", Diff, None),
    ]);
}

pub struct WFilesApp {
    ops: Entity<Ops>,
    /// 缩略图缓存：所有标签共用一份，同一个文件只解一次。
    thumbs: Entity<Thumbnails>,
    tabs: Vec<Entity<Pane>>,
    /// 焦点所在的标签。双栏时两栏各显示一个标签，焦点决定命令作用于谁。
    active: usize,
    /// 双栏时**另一栏**显示哪个标签；None = 单栏。
    split: Option<usize>,
    /// 双栏时焦点那一栏是不是在左边。单独记这一位是为了让两栏位置稳定：
    /// 换焦点只是挪焦点，不会让这一栏跳到对面去。
    focus_on_left: bool,
    /// 双栏同步浏览：一边换目录，另一边跟着走。
    sync: bool,
    /// 输入对话框（新建文件夹/重命名）的输入框，随开随建。
    prompt_input: Option<Entity<InputState>>,
    /// 批量重命名对话框的输入框组。
    bulk_inputs: Option<BulkInputs>,
    /// 命令面板状态与输入框。
    palette: Option<PaletteState>,
    palette_input: Option<Entity<InputState>>,
    /// 最近一次落盘的会话，用来判断要不要再写一次。
    saved_session: Session,
    /// 当前窗口的位置与尺寸，退出时存进设置。
    layout: Option<Layout>,
    /// 这一扇是不是主窗。副窗**只读不写**：会话、窗口布局、崩溃现场
    /// 都归主窗（理由见 [`Starting`]）。
    primary: bool,
    /// 退出时存盘的订阅；必须一直持有，否则回调会被丢弃。
    quit_subscription: Option<Subscription>,
    subscriptions: Vec<Subscription>,
    /// 底部的内嵌终端。`None` = 收起来；收起来时实体也一起丢掉，
    /// 于是里面的 shell 一定会被杀掉（见 `Terminal` 的 `Drop`）。
    term: Option<Entity<Terminal>>,
    /// 比较面板。`None` = 收起来；收起来时实体一起丢掉，后台那一趟随之停下。
    compare: Option<Entity<ComparePanel>>,
    /// 文件内容比较面板。
    diff: Option<Entity<DiffPanel>>,
    /// 元数据列的缓存与后台探测：所有标签共用一份，同一个文件只探一次。
    meta: Entity<Metadata>,
    /// 远程连接的注册表：握着 WebDAV 客户端、目录缓存与下载任务。
    remotes: Entity<Remotes>,
    /// 连接对话框。`None` = 关着；关掉时输入框实体一起丢掉。
    connect: Option<ConnectState>,
    /// 侧栏里哪条连接的“删除”被按下了一次（两步确认的第一步）。
    forget_armed: Option<String>,
    /// 侧栏收成图标条。展开 210px、收起 48px。
    ///
    /// 这不是可有可无的装饰：侧栏原本写死 210px 不可收缩，而工具栏上
    /// 一排按钮要 908px。窗口窄到 900px 时内容区只剩 430px，工具栏右半边
    /// （筛选框、视图切换）整个画到屏幕外——看得见却点不到。给用户一个
    /// 收起侧栏的开关，是让窄窗口可用最省事的一步。
    sidebar_collapsed: bool,
    /// 最近一次连上后，密码到底落在哪——必须让用户看得见，    /// 否则“密码明文躺在设置里”这种事没人会知道。
    secret_note: Option<String>,
}

/// 这一次开窗的来路：命令行给了什么、这一扇是什么身份。
///
/// 打包成一个结构体而不是给 [`WFilesApp::new`] 加三个参数：
/// `fresh` 与 `primary` 都是"是/否"，两个并排的 `bool` 传起来必然会有人传反。
#[derive(Clone, Debug, PartialEq)]
pub struct Starting {
    /// 命令行说开到哪儿。
    pub launch: Option<cli::Launch>,
    /// 从主页起，不恢复上次的会话。
    ///
    /// 只有 `--new-window` 且另有一份开着时要这个：用户要的是"给我一扇干净的
    /// 窗看着这个地方"，不是"把上次那排标签再复制一份"。
    pub fresh: bool,
    /// 这一扇是不是主窗（抢到了位子）。
    ///
    /// 副窗**只读不写**：会话、窗口布局、崩溃恢复现场都归主窗。三样东西
    /// 各自只有一份文件，两扇窗都写就是互相覆盖——
    /// 会话被覆盖的症状是"我的标签怎么少了一半"，而那时已经无从查起。
    /// 副窗还会写的是**按目录**的选中现场（`restore.json`）：那不是谁的
    /// 身份，是"这个目录上次选了什么"，两扇窗看着同一个目录时本来就只能存一份。
    pub primary: bool,
}

impl Starting {
    /// 主窗：正常启动的那一扇（抢到了位子，会话与布局都归它）。
    pub fn primary(launch: Option<cli::Launch>) -> Self {
        Self {
            launch,
            fresh: false,
            primary: true,
        }
    }
}

impl WFilesApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, starting: Starting) -> Self {
        let Starting {
            launch,
            fresh,
            primary,
        } = starting;
        // 设置要在建窗格之前装好：窗格渲染时要读“默认终端”。
        Settings::install(cx);
        let remotes = cx.new(|_| Remotes::new());
        // 操作层要认得远程连接：拷贝/移动/撤销都可能跨本地和远程。
        let ops = cx.new(|_| Ops::new(Some(remotes.clone())));
        let thumbs = cx.new(|_| Thumbnails::new());
        let meta = cx.new(|_| Metadata::new());

        // 恢复上次的标签；一个都不剩就开主页。
        //
        // `fresh`（`--new-window` 要的那种"新"窗）一份都不恢复：用户要的是
        // 一扇干净的窗看着某个地方，不是把上次那排标签再复制一份。
        let session = if fresh {
            Session::default()
        } else {
            Session::load().sanitized()
        };
        // 命令行指定的目标，算成"开哪个目录 + 选中哪一个"。
        // 规则在 `cli` 里（裸路径给目录就开它、给文件就定位到它，
        // `--reveal` 一律定位），那边有测试。
        let opening = launch
            .as_ref()
            .map(|it| it.target.resolve(Self::on_disk(it.target.path())));

        // 这一次要开哪几栏。
        let mut openings: Vec<(PathBuf, Option<PathBuf>)> = session
            .tabs
            .iter()
            .map(|path| (path.clone(), None))
            .collect();
        match (&launch, opening) {
            // `--new-tab`：多开一栏，别的照旧。
            (Some(it), Some(open)) if it.new_tab => openings.push((open.dir, open.select)),
            // 默认：把**当前那一栏**换成目标，其余照常恢复。不先恢复再跳——
            // 那会闪一下，远程路径还得白连一趟。
            (Some(_), Some(open)) if !openings.is_empty() => {
                let active = session.active.min(openings.len() - 1);
                openings[active] = (open.dir, open.select);
            }
            // 会话是空的（或者标签全指向已经不存在的目录）：目标就是全部。
            (_, Some(open)) => openings.push((open.dir, open.select)),
            (_, None) => {}
        }
        if openings.is_empty() {
            openings.push((Self::initial_dir(), None));
        }

        let mut tabs: Vec<Entity<Pane>> = Vec::new();
        for (dir, select) in openings {
            let tab = cx.new(|cx| {
                let mut pane = Pane::new(
                    dir,
                    ops.clone(),
                    thumbs.clone(),
                    meta.clone(),
                    remotes.clone(),
                    window,
                    cx,
                );
                // 命令行说"定位到这个文件"：选中它、滚到它。这条压过恢复
                // 现场——用户刚敲的命令比上次退出时的现场新。
                if let Some(path) = &select {
                    pane.select_path(path, cx);
                }
                pane
            });
            tabs.push(tab);
        }
        let active = match &launch {
            // `--new-tab` 开出来的那一栏就是焦点所在。
            Some(it) if it.new_tab => tabs.len() - 1,
            _ => session.active.min(tabs.len() - 1),
        };
        let split = session
            .split
            .filter(|&index| index < tabs.len() && index != active);

        let mut app = Self {
            ops: ops.clone(),
            thumbs,
            tabs,
            active,
            split,
            focus_on_left: split.is_none_or(|_| session.focus_on_left),
            sync: split.is_some() && session.sync,
            prompt_input: None,
            bulk_inputs: None,
            palette: None,
            palette_input: None,
            saved_session: Session::default(),
            layout: None,
            primary,
            quit_subscription: None,
            subscriptions: Vec::new(),
            term: None,
            compare: None,
            diff: None,
            meta,
            remotes,
            connect: None,
            forget_armed: None,
            sidebar_collapsed: false,
            secret_note: None,
        };

        // 侧栏的「收藏」段读的是设置，而设置是**窗格**改的（Cmd+D 在窗格上）。
        // 窗格 notify 只重画窗格，侧栏在 App 上，不重画就纹丝不动。
        //
        // 这里观察**设置这个全局**而不是等窗格来报：`Settings::update`
        // 走 GPUI 的 `update_global`，它会推一个 `NotifyGlobalObservers`
        // effect（见 gpui-pre 的 `default_global`），观察者一律被叫醒。
        // 好处是**以后再加设置字段自动生效**——侧栏、工具栏、详情板
        // 都从设置里读东西，靠"记得在每个改动处通知 App"必然漏一处，
        // 而漏掉的那处症状是"改了设置界面没反应"，极难定位。
        //
        // `Subscription` 存在 `subscriptions` 里（不能 `let _ =` 丢掉）：
        // 它一析构就注销，整条接线静默失效——而症状跟没接线一模一样
        // （见下面 `settings_changes_wake_the_app` 那条测试的注释）。
        app.subscriptions
            .push(cx.observe_global::<Settings>(|_window, cx| cx.notify()));
        app.subscriptions.push(cx.subscribe_in(
            &ops,
            window,
            |this, _, event: &OpsEvent, window, cx| this.on_ops_event(event, window, cx),
        ));
        for tab in &app.tabs {
            app.subscriptions.push(cx.subscribe_in(
                tab,
                window,
                |this, pane, event: &PaneEvent, window, cx| {
                    this.on_pane_event(pane, event, window, cx);
                },
            ));
        }
        // 远程：连上就跳过去，列完目录由窗格自己处理（`Remotes::entries`），
        // 这里只管"连上了 / 没连上 / 打开失败"这三件需要告诉用户的事。
        let remotes = app.remotes.clone();
        app.subscriptions.push(cx.subscribe_in(
            &remotes,
            window,
            |this, _, event: &RemotesEvent, window, cx| this.on_remotes_event(event, window, cx),
        ));
        app.saved_session = app.snapshot(cx);
        // 会话恢复了哪几个标签，就只留那几个目录的恢复现场。
        // 这条要在 `saved_session` 之后：清扫要用 `self.tabs`，那已经是最终形态。
        app.sweep_restore(cx);

        // 键盘交给活动的那一栏。上面每一栏建出来都会自己抓一次焦点，于是
        // **最后建的那一栏**赢了下来——而活动栏不一定是它（会话里 `active`
        // 常常在中间）。抓错地方的后果不是"多按一下"：焦点所属的那一栏不在
        // 渲染树里，应用级快捷键会静默失效，用户看见的是"刚打开就按不出
        // Cmd+W / Cmd+T"，点一下界面才好（见 `hand_keyboard_to_active_pane`）。
        app.hand_keyboard_to_active_pane(window, cx);

        // 退出前再存一次，覆盖“最后一次导航还没来得及落盘”的窗口。
        app.quit_subscription = Some(cx.on_app_quit(|this, cx| {
            this.sync_session(cx);
            this.save_layout(cx);
            std::future::ready(())
        }));

        app
    }

    /// 当前焦点那一栏选中了哪些文件（按列表顺序）。
    ///
    /// 面板类实体该有的读取口：崩溃恢复的集成测试要断言"重开之后
    /// 那批选中还在"，而界面上读不到选中状态（选中标在不可观察的
    /// `ListItem` 上）。
    #[cfg(test)]
    pub fn selected_in_active_pane(&self, cx: &gpui_kit::App) -> Vec<PathBuf> {
        self.tabs
            .get(self.active)
            .map(|tab| tab.read(cx).selected_paths())
            .unwrap_or_default()
    }

    /// 开着的那几个目录，按窗格实际认到的形式（不是传进来的原样字符串）。
    ///
    /// 测试用：`tempfile` 在 macOS 上给的是 `/var/folders/...`，而窗格
    /// 那边可能是 `/private/var/folders/...`（前者是符号链接）。
    /// 拿原样路径去比对，断言会因为"路径字符串不一样"而失败，
    /// 看起来像功能坏了，其实是两个字符串指向同一个目录。
    #[cfg(test)]
    pub fn tab_paths(&self, cx: &gpui_kit::App) -> Vec<PathBuf> {
        self.tabs
            .iter()
            .map(|tab| tab.read(cx).path().to_path_buf())
            .collect()
    }

    /// 记下窗口现在的位置与尺寸。
    ///
    /// 在渲染时取而不是等到退出时：退出回调拿不到 `Window`，
    /// 而渲染每一帧都有窗口句柄，顺便读一下是零成本的。
    fn capture_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 副窗不记位置：它记下去就是替主窗把窗口挪了——下次启动主窗开在副窗
        // 当时那个地方（而副窗可能早就关了）。
        if !self.primary {
            return;
        }
        let bounds = window.window_bounds().get_bounds();
        let layout = Layout {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        };

        // 只在真的变了的时候写盘：拖动窗口不会每一帧落一次文件，
        // 但崩溃退出也不会丢掉布局（不必等到正常的退出流程）。
        if self.layout != Some(layout) {
            self.layout = Some(layout);
            Settings::update(cx, |settings| settings.layout = Some(layout));
        }
    }

    /// 退出前把布局写进设置。记不下来无所谓，下次用默认尺寸。
    fn save_layout(&self, cx: &mut App) {
        // **没记过就不写**：副窗从没记过布局（`capture_layout` 拦掉了），
        // 而把一个 `None` 写下去等于把主窗记着的位置抹了——下次启动
        // 主窗就从默认大小居中开，用户只会觉得"它自己乱跳"。
        let Some(layout) = self.layout else {
            return;
        };
        Settings::update(cx, |settings| settings.layout = Some(layout));
    }

    /// 磁盘上这个是目录、是文件，还是已经不在了。
    ///
    /// 只回答"怎么开"（见 [`cli::Target::resolve`]），不回答"认不认这个参数"。
    /// 跟着符号链接走：`wfiles ~/桌面/某个链接` 指的是链接指向的那个目录，
    /// 打开它比在桌面里选中它更接近用户的意思——真要选中链接本身，
    /// `--reveal` 就在那儿。
    fn on_disk(path: &Path) -> cli::OnDisk {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => cli::OnDisk::Dir,
            Ok(_) => cli::OnDisk::File,
            Err(_) => cli::OnDisk::Missing,
        }
    }

    fn initial_dir() -> PathBuf {
        dirs::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn places() -> Vec<(&'static str, IconName, PathBuf)> {
        let mut places = Vec::new();
        if let Some(home) = dirs::home_dir() {
            places.push(("个人文件夹", IconName::House, home));
        }
        if let Some(desktop) = dirs::desktop_dir() {
            places.push(("桌面", IconName::Monitor, desktop));
        }
        if let Some(documents) = dirs::document_dir() {
            places.push(("文稿", IconName::FileText, documents));
        }
        if let Some(downloads) = dirs::download_dir() {
            places.push(("下载", IconName::Download, downloads));
        }
        places
    }

    /// 一批文件落到了这个目录里。
    ///
    /// 和窗格里那套是同一条链路（同样先预检冲突、同样走后台操作中心），
    /// 只是发起方换成了标签栏或侧边栏。
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
        let action = drag::drop_action(from_outside, window.modifiers().alt);
        self.ops.update(cx, |ops, cx| {
            ops.drop_into(paths, destination, action == drag::DropAction::Move, cx)
        });
        cx.notify();
    }

    /// 把一个元素变成投放目标：落下来的文件进 `destination`。
    ///
    /// 标签页和侧边栏位置说到底都是"一个目录"，所以共用这一份接线。
    fn drop_target<E>(element: E, destination: PathBuf, cx: &mut Context<Self>) -> AnyElement
    where
        E: StatefulInteractiveElement + Styled + IntoElement + 'static,
    {
        let guard = destination.clone();
        let mut element = element.can_drop(move |value, _, _| {
            crate::pane::dropped_paths(value).is_some_and(|paths| drag::can_drop(&guard, paths))
        });
        let internal = destination.clone();
        element = element.on_drop(cx.listener(move |this, drag: &DraggedPaths, window, cx| {
            let paths = drag.paths.to_vec();
            this.drop_paths(paths, internal.clone(), false, window, cx);
        }));
        element = element.on_drop(cx.listener(move |this, drag: &ExternalPaths, window, cx| {
            let paths = drag.0.iter().cloned().collect();
            this.drop_paths(paths, destination.clone(), true, window, cx);
        }));
        element.into_any_element()
    }

    fn navigate_active_tab(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs[self.active].update(cx, |pane, cx| pane.navigate_to(path, true, window, cx));
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.sidebar_collapsed {
            return self.render_sidebar_collapsed(cx);
        }
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let items = Self::places()
            .into_iter()
            .enumerate()
            .map(|(index, (label, icon, path))| {
                let target = path.clone();
                // 查一次，标签与无障碍名共用：两边不一致的话读屏软件会念出
                // 一个界面上根本不存在的词条，而用户看不见它错在哪。
                let label = crate::i18n::tr(cx, label);
                let item = ListItem::new(("place", index))
                    .rounded(cx.theme().radius)
                    .selected(current == path)
                    .accessibility_label(label.clone())
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Icon::new(icon).size_4())
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_active_tab(target.clone(), window, cx);
                    }));
                // 拖到侧边栏的位置上就是放进那个位置：
                // `ListItem` 只实现了 `IntoElement`，挂不上拖放，外面套一层 div。
                Self::drop_target(
                    div().id(("place-drop", index)).child(item),
                    path.clone(),
                    cx,
                )
            })
            // 马上收成 Vec：闭包里要用到 `cx`（挂投放回调），
            // 懒迭代器会把 `cx` 一直借着，下面的 `cx.theme()` 就借不到了。
            .collect::<Vec<_>>();

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    // 40 不是随手取的：右侧地址栏那一行就是 40 px。
                    // 两边不等高的话，右侧第二行（工具栏）会在侧栏这条
                    // 分隔线下面露出来，看着像错位。
                    .h(px(40.))
                    .px_3()
                    .gap_3()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .size_7()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .child(Icon::new(IconName::FolderOpen).size_4()),
                    )
                    .child(
                        v_flex().child(div().font_semibold().child("WFiles")).child(
                            div()
                                .id("sidebar-tagline")
                                .test_support()
                                .aria_label(crate::i18n::tr(cx, "快速、清楚、可撤销"))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::i18n::tr(cx, "快速、清楚、可撤销")),
                        ),
                    )
                    .child(
                        Button::new("sidebar-expand")
                            .icon(IconName::PanelLeftClose)
                            .ghost()
                            .small()
                            .ml_auto()
                            .tooltip(crate::i18n::tr(cx, "收起侧栏（Cmd+B）"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_collapsed = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        div()
                            .id("sidebar-places-title")
                            .test_support()
                            .aria_label(crate::i18n::tr(cx, "位置"))
                            .px_3()
                            .py_2()
                            .text_xs()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::i18n::tr(cx, "位置")),
                    )
                    .children(items),
            )
            .child(self.render_favorites_section(cx))
            .child(self.render_volumes_section(cx))
            .child(self.render_cloud_section(cx))
            .child(self.render_remote_section(cx))
            .child(div().flex_1())
            .child(
                h_flex()
                    .id("sidebar-local")
                    .test_support()
                    .aria_label(crate::i18n::tr(cx, "本地文件系统"))
                    .m_2()
                    .p_3()
                    .gap_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(Icon::new(IconName::HardDrive).size_4())
                    .child(crate::i18n::tr(cx, "本地文件系统")),
            )
            .into_any_element()
    }

    /// 侧栏收起来之后的样子：一条 48px 的图标条。
    ///
    /// 收起时**位置入口一个都不少**——只留一颗展开按钮的话，用户收起之后
    /// 就找不到"个人文件夹/桌面/下载"了，等于把侧栏废掉。这些图标条上的
    /// 每一项都还能点回去，标签写不进就靠 tooltip 与无障碍名。
    fn render_sidebar_collapsed(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let mut places: Vec<AnyElement> = Self::places()
            .into_iter()
            .enumerate()
            .map(|(index, (label, icon, path))| {
                let target = path.clone();
                // 同上：tooltip 与无障碍名共用一句翻译。
                let label = crate::i18n::tr(cx, label);
                Button::new(("collapsed-place", index))
                    .icon(icon)
                    .ghost()
                    .small()
                    .selected(current == path)
                    .tooltip(label.clone())
                    .accessibility_label(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_active_tab(target.clone(), window, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        // 收藏也一个都不少（同上那条理由）：用户自己收藏的东西比
        // "桌面/文稿"更是他每天要点的地方，收起侧栏就够不着它们
        // 等于把收藏功能废掉了一半。
        let favorites = Settings::peek(cx, |settings| settings.favorites.clone());
        if !favorites.is_empty() {
            places.push(
                div()
                    .id("collapsed-favorites-gap")
                    .size(px(4.))
                    .into_any_element(),
            );
            places.extend(favorites.iter().enumerate().map(|(index, favorite)| {
                let target = favorite.path.clone();
                let is_dir = favorite.is_dir;
                let label = crate::pane::Pane::short_name_of(&favorite.path);
                Button::new(("collapsed-favorite", index))
                    .icon(if is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .ghost()
                    .small()
                    .selected(current == favorite.path)
                    .tooltip(label.clone())
                    .accessibility_label(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_favorite(target.clone(), is_dir, window, cx);
                    }))
                    .into_any_element()
            }));
        }

        v_flex()
            .w(px(SIDEBAR_COLLAPSED_WIDTH))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .py_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                Button::new("sidebar-collapse")
                    .icon(IconName::PanelLeftOpen)
                    .ghost()
                    .small()
                    .tooltip(crate::i18n::tr(cx, "展开侧栏（Cmd+B）"))
                    .accessibility_label(crate::i18n::tr(cx, "展开侧栏"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_collapsed = false;
                        cx.notify();
                    })),
            )
            .child(div().h(px(4.)))
            .children(places)
            .into_any_element()
    }

    /// 侧栏的「磁盘」区：本机挂着的卷（外接盘、U 盘、网络共享）一条一行。
    ///
    /// 一个卷都没挂的时候整块都不出现——跟云盘那一条一个道理，
    /// 空分组比没有更烦人。
    ///
    /// 排在云盘上面：插 U 盘找它是眼下最急的一件事，不该排在云盘后面。
    /// 侧栏的「收藏」段：用户自己收藏的文件夹与文件。
    ///
    /// **一项都没收藏时整块不出现**——跟磁盘、云盘那两段一个道理：
    /// 空分组比没有更烦人。
    ///
    /// 文件和文件夹都收（用户 2026-10-14 拍板）：点文件夹是进去，
    /// 点文件是交给系统打开，行为跟列表里双击一样。收藏的路径被删了
    /// 也照样列着——用户明确选了"点进去才报"，静默把配置里的条目删掉
    /// 是最难查的一类故障。
    fn render_favorites_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let favorites = Settings::peek(cx, |settings| settings.favorites.clone());
        if favorites.is_empty() {
            return div().into_any_element();
        }
        let current = self.tabs[self.active].read(cx).path().to_path_buf();

        let rows: Vec<AnyElement> = favorites
            .iter()
            .enumerate()
            .map(|(index, favorite)| {
                let target = favorite.path.clone();
                let is_dir = favorite.is_dir;
                // 收藏项的名字现算：用户把文件夹改了名，这里要跟着变。
                let label = crate::pane::Pane::short_name_of(&favorite.path);
                let row = ListItem::new(("favorite", index))
                    .rounded(cx.theme().radius)
                    .selected(current == favorite.path)
                    .accessibility_label(label.clone())
                    .child(
                        h_flex()
                            .gap_3()
                            // 文件夹与文件给不同的图标：侧栏这一段的图标
                            // 是唯一的类型提示（行里只有一行字，没有扩展名）。
                            .child(
                                Icon::new(if is_dir {
                                    IconName::Folder
                                } else {
                                    IconName::File
                                })
                                .size_4(),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(label)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_favorite(target.clone(), is_dir, window, cx);
                    }));
                // 拖到收藏项上 = 拷进去，跟拖到"位置"里的"下载"同理。
                Self::drop_target(
                    div().id(("favorite-drop", index)).child(row),
                    favorite.path.clone(),
                    cx,
                )
                .into_any_element()
            })
            .collect();

        v_flex()
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .id("sidebar-favorites")
                    .test_support()
                    .aria_label(crate::i18n::tr(cx, "收藏夹"))
                    .child(crate::i18n::tr(cx, "收藏夹")),
            )
            .children(rows)
            .into_any_element()
    }

    /// 点了一个收藏项：文件夹进去，文件交给系统打开。
    ///
    /// **按收藏那一刻记住的 `is_dir` 决定，不去问磁盘**：路径被删掉之后
    /// `is_dir()` 会说 false，于是点一个消失了的目录会去叫系统打开它，
    /// 报出来的是"没有应用能打开"——那是在误导用户（用户看到的是一个
    /// 明明是文件夹的东西）。
    ///
    /// 打开失败的话转给当前窗格去报（[`Pane::open_favorite`]）：状态栏
    /// 那一句是窗格的字段，App 自己没有第二套错误通路可写。
    fn open_favorite(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tabs[self.active].update(cx, |pane, cx| {
            pane.open_favorite(path, is_dir, window, cx);
        });
    }

    fn render_volumes_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let volumes = crate::volumes::detected();
        if volumes.is_empty() {
            return div().into_any_element();
        }

        let rows: Vec<AnyElement> = volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| {
                let target = volume.path.clone();
                let spoken = volume.spoken();
                let row = ListItem::new(("volume", index))
                    .rounded(cx.theme().radius)
                    .selected(current == volume.path)
                    .accessibility_label(spoken)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Icon::new(IconName::HardDrive).size_4())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(volume.name.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_active_tab(target.clone(), window, cx);
                    }));
                // 拖到某个卷上 = 拷进去，跟拖到"位置"里的"下载"是一个道理。
                Self::drop_target(
                    div().id(("volume-drop", index)).child(row),
                    volume.path.clone(),
                    cx,
                )
                .into_any_element()
            })
            .collect();

        v_flex()
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .id("sidebar-disks")
                    .test_support()
                    .aria_label(crate::i18n::tr(cx, "磁盘"))
                    .child(crate::i18n::tr(cx, "磁盘")),
            )
            .children(rows)
            .into_any_element()
    }

    /// 侧栏的"云盘"区：本机上找得到的同步文件夹，一条一行。
    ///
    /// 没有装任何云盘客户端时这一整块都不出现——空分组比没有更烦人。
    fn render_cloud_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let roots = crate::cloud::detected();
        if roots.is_empty() {
            return div().into_any_element();
        }

        let rows: Vec<AnyElement> = roots
            .iter()
            .enumerate()
            .map(|(index, root)| {
                let target = root.path.clone();
                let label = root.label();
                let name = root.provider.name();
                // 读屏要听得到"这是谁的同步文件夹"，光听文件夹名不知道。
                // 名字跟厂商名一样时就别念两遍。
                let spoken = if label == name {
                    name.to_string()
                } else {
                    format!("{name}：{label}")
                };
                let row = ListItem::new(("cloud", index))
                    .rounded(cx.theme().radius)
                    .selected(current == root.path)
                    .accessibility_label(spoken)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Icon::new(IconName::Cloud).size_4())
                            .child(div().flex_1().min_w_0().truncate().child(label)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_active_tab(target.clone(), window, cx);
                    }));
                row.into_any_element()
            })
            .collect();

        v_flex()
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .id("sidebar-cloud")
                    .test_support()
                    .aria_label(crate::i18n::tr(cx, "云盘"))
                    .child(crate::i18n::tr(cx, "云盘")),
            )
            .children(rows)
            .into_any_element()
    }

    /// 侧栏的"远程"区：存下来的连接一条一行，末尾一行是"连接服务器"。
    ///
    /// 删连接没有放进命令面板，只在侧栏做：它要点两下（第一下只是举手），
    /// 而面板里一条 Enter 就删掉一个记不住的密码太容易误伤。
    fn render_remote_section(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let saved = Settings::peek(cx, |settings| settings.remotes.clone());
        let armed = self.forget_armed.clone();
        let note = self.secret_note.clone();

        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, config) in saved.iter().enumerate() {
            let id = config.id.clone();
            let name = config.name.clone();
            let target = remote::location(&config.id, &config.root);
            let is_current = current == target;
            let is_armed = armed.as_deref() == Some(id.as_str());
            let open_id = id.clone();
            let forget_id = id.clone();

            let row = ListItem::new(("remote", index))
                .rounded(cx.theme().radius)
                .selected(is_current)
                .accessibility_label(name.clone())
                .child(
                    h_flex()
                        .gap_3()
                        .child(Icon::new(IconName::Cloud).size_4())
                        .child(div().flex_1().min_w_0().truncate().child(name.clone())),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_remote(open_id.clone(), window, cx)
                }));
            rows.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(row))
                    // 举起手之后才给"确认删除"，手放下去就只是个叉。
                    .child(
                        Button::new(("remote-forget", index))
                            .label(if is_armed { "确认删除" } else { "×" })
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.forget_armed.as_deref() == Some(forget_id.as_str()) {
                                    this.forget_remote(&forget_id, window, cx);
                                } else {
                                    this.forget_armed = Some(forget_id.clone());
                                    cx.notify();
                                }
                            })),
                    )
                    .into_any_element(),
            );
        }
        rows.push(
            ListItem::new("remote-new")
                .rounded(cx.theme().radius)
                .accessibility_label(crate::i18n::tr(cx, "连接服务器"))
                .child(
                    h_flex()
                        .gap_3()
                        .child(Icon::new(IconName::Plus).size_4())
                        .child(crate::i18n::tr(cx, "连接服务器…")),
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_connect(window, cx)))
                .into_any_element(),
        );

        v_flex()
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .id("sidebar-remote")
                    .test_support()
                    .aria_label(crate::i18n::tr(cx, "远程"))
                    .child(crate::i18n::tr(cx, "远程")),
            )
            .children(rows)
            .when_some(note, |this, note| {
                this.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(note),
                )
            })
    }

    // ---- 标签管理 ----

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let home = Self::initial_dir();
        self.add_tab(home, window, cx);
    }

    /// 开一个新标签并让焦点落到它上面。返回它的下标。
    fn add_tab(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> usize {
        // 模态开着的时候新栏不能抢键盘：`Pane::new` 会自己抓一次焦点，把用户
        // 正打字的那个框甩掉——面板从此"看着开着、其实按不动"（打不进字，
        // 连 Esc 都送不进去，只能点一下别的界面把它弄回来）。先记下键盘此刻
        // 在谁手上，建完还回去（见 `hand_keyboard_to_active_pane`）。
        let keyboard = self.modal_open().then(|| window.focused(cx)).flatten();
        let tab = cx.new(|cx| {
            Pane::new(
                path,
                self.ops.clone(),
                self.thumbs.clone(),
                self.meta.clone(),
                self.remotes.clone(),
                window,
                cx,
            )
        });
        self.subscriptions.push(cx.subscribe_in(
            &tab,
            window,
            |this, pane, event: &PaneEvent, window, cx| {
                this.on_pane_event(pane, event, window, cx);
            },
        ));
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        if let Some(handle) = keyboard {
            // 新栏建好了，键盘还给刚才那个模态的输入框（没人抢的话它一直在）。
            handle.focus(window, cx);
        }
        self.sync_session(cx);
        cx.notify();
        self.active
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            // 最后一个标签关掉就重置回主页，窗口不空着。
            let home = Self::initial_dir();
            self.tabs[0].update(cx, |pane, cx| pane.reset_to(home, window, cx));
            self.active = 0;
            self.hand_keyboard_to_active_pane(window, cx);
            self.sync_session(cx);
            cx.notify();
            return;
        }

        // 先记住要关掉的是哪个目录：标签一删就问不出来了，而恢复文件
        // 里那条现场得跟着一起走——留着的话下次碰巧打开同一个目录，
        // 会冒出一批用户早就忘了的选中项。
        let closed = self.tabs[index].read(cx).path().to_path_buf();
        self.forget_restore(&closed);

        self.tabs.remove(index);
        // 两个下标都要跟着挪；另一栏正好被关掉就退回单栏。
        self.split = match self.split {
            Some(partner) if partner == index => None,
            Some(partner) => Some(if partner > index {
                partner - 1
            } else {
                partner
            }),
            None => None,
        };
        if self.active > index {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        if self.split.is_none() {
            self.focus_on_left = true;
            self.sync = false;
        }
        // 关掉的那一栏连同它的实体一起没了，焦点留在它身上就是"没主"
        // （见 `hand_keyboard_to_active_pane`）。
        self.hand_keyboard_to_active_pane(window, cx);
        self.sync_session(cx);
        cx.notify();
    }

    /// 有没有"自己拿着键盘"的模态开着：命令面板与几个带输入框的对话框。
    ///
    /// 它们开着的期间换栏、关栏都不该把键盘抢回来——用户正在里面打字。
    fn modal_open(&self) -> bool {
        self.palette.is_some()
            || self.prompt_input.is_some()
            || self.bulk_inputs.is_some()
            || self.connect.is_some()
    }

    /// 键盘交给当前那一栏。
    ///
    /// 这不是锦上添花，是**快捷键还能不能按出来的前提**：焦点所属的元素一旦
    /// 不在渲染树里（切栏、关栏、浮层的输入框被放下），gpui 找不着它，就把按键
    /// 派发到**窗口根节点**——挂在应用根元素上的 `on_action` 一个都不在那条路径
    /// 上。于是切栏、关栏、开标签、命令面板、分栏……全按不出来，而且不报错、
    /// 不留痕：用户只看见"键盘突然不灵了"，点一下界面又好了。
    ///
    /// 模态开着时不抢：那种时候键盘该留在用户正打字的那个框里。
    /// 换栏/关栏走这里，关掉浮层的人（[`Self::close_palette`]、
    /// [`Self::close_connect`]、[`Self::on_ops_event`]）也走这里。
    fn hand_keyboard_to_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        self.tabs[self.active].update(cx, |pane, cx| pane.focus(window, cx));
    }

    fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let mut next = (self.active + 1) % self.tabs.len();
        // 一轮撞上另一栏就再往前一个；两栏各占一个标签时没得可换，不动。
        if Some(next) == self.split {
            next = (next + 1) % self.tabs.len();
            if next == self.active {
                return;
            }
        }
        self.active = next;
        // 键盘跟着走：换掉的那一栏下一帧就不在渲染树里了（见上）。
        self.hand_keyboard_to_active_pane(window, cx);
        self.sync_session(cx);
        cx.notify();
    }

    // ---- 面板显隐 ----

    /// 收起 / 展开侧栏。
    ///
    /// 不存进设置：这不是偏好，是"这块屏幕现在有多宽"的应对。
    /// 下次开在一个大屏上还给用户一条 48px 的图标条，等于替他们做了
    /// 一个他们没要求的决定。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    /// 显示 / 隐藏详情板。
    ///
    /// 只动**当前显示的那几栏**：双栏时两栏可能看的是不同的东西，
    /// 一条命令把两边一起收掉会让另一栏的预览莫名其妙地消失。
    fn toggle_details(&mut self, cx: &mut Context<Self>) {
        for index in self.visible() {
            self.tabs[index].update(cx, |pane, cx| {
                pane.details_shown = !pane.details_shown;
                cx.notify();
            });
        }
    }

    /// 焦点那一栏此刻有没有详情板——命令面板要拿它决定条目写"收起"还是"展开"。
    fn details_shown_in_active(&self, cx: &Context<Self>) -> bool {
        self.tabs[self.active].read(cx).details_shown
    }

    // ---- 双栏 ----

    /// 此刻显示哪两个标签，按屏幕上的左右顺序。
    fn visible(&self) -> Vec<usize> {
        match self.split {
            Some(partner) if self.focus_on_left => vec![self.active, partner],
            Some(partner) => vec![partner, self.active],
            None => vec![self.active],
        }
    }

    fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.take().is_some() {
            self.focus_on_left = true;
            self.sync = false;
            self.sync_session(cx);
            cx.notify();
            return;
        }

        let partner = if self.tabs.len() >= 2 {
            (self.active + 1) % self.tabs.len()
        } else {
            // 只有一个标签：复制一份出来当另一栏，两边先看同一个目录，
            // 用户再自己导航其中一边——这比凭空跳到主目录有用。
            let here = self.tabs[self.active].read(cx).path().to_path_buf();
            self.add_tab(here, window, cx);
            let created = self.active;
            self.active = 0;
            created
        };
        self.split = Some(partner);
        self.focus_on_left = true;
        self.sync_session(cx);
        cx.notify();
    }

    /// 把焦点换到另一栏，两栏在屏幕上的位置不动。
    fn focus_other(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_partner(cx);
        self.tabs[self.active].update(cx, |pane, cx| pane.focus(window, cx));
    }

    /// 交换"焦点栏"和"另一栏"两个下标，同时翻左右标志位——
    /// 两个动作合起来才让位置保持不变。
    fn focus_partner(&mut self, cx: &mut Context<Self>) {
        let Some(partner) = self.split else {
            return;
        };
        let previous = self.active;
        self.active = partner;
        self.split = Some(previous);
        self.focus_on_left = !self.focus_on_left;
        self.sync_session(cx);
        cx.notify();
    }

    fn toggle_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.is_none() {
            return;
        }
        self.sync = !self.sync;
        // 一打开就把另一栏拉到同一个目录：开关的意义是"从现在起一起走"。
        if self.sync {
            self.mirror_navigation(&self.tabs[self.active].clone(), window, cx);
        }
        self.sync_session(cx);
        cx.notify();
    }

    /// 开关底部的内嵌终端面板。
    ///
    /// 打开时起在当前窗格（或唯一选中的那个目录）里；收起来就把实体丢掉，
    /// 于是里面的 shell 一定跟着没——不留下一个看不见还在跑的进程。
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.term.is_some() {
            self.term = None;
            // 键盘在终端手里，收起来得有人接手（见 `hand_keyboard_to_active_pane`）。
            self.hand_keyboard_to_active_pane(window, cx);
            cx.notify();
            return;
        }
        let dir = self.tabs[self.active].read(cx).terminal_target();
        let terminal = cx.new(|cx| Terminal::new(dir, cx));
        self.subscriptions.push(cx.subscribe_in(
            &terminal,
            window,
            |this, _, event: &TerminalEvent, window, cx| match event {
                TerminalEvent::Close => {
                    this.term = None;
                    // shell 自己退出时用户可能正等着继续敲键盘：还回去。
                    this.hand_keyboard_to_active_pane(window, cx);
                    cx.notify();
                }
            },
        ));
        // 一开就把键盘交过去：打开终端却还要点一下才能打字，那一下很别扭。
        let handle = terminal.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.term = Some(terminal);
        cx.notify();
    }

    /// 把选中项拷（或移）到另一栏正在看的目录。
    ///
    /// 走的是和 Cmd+C / Cmd+X 同一条路：先放剪贴板再粘贴，
    /// 所以冲突预检、撤销、进度全都照旧。
    fn transfer_to_other(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(partner) = self.split else {
            return;
        };
        let paths = self.tabs[self.active].read(cx).selected_paths();
        if paths.is_empty() {
            return;
        }
        let dest = self.tabs[partner].read(cx).path().to_path_buf();
        self.ops.update(cx, |ops, cx| {
            if cut {
                ops.cut(paths, cx)
            } else {
                ops.copy(paths, cx)
            }
        });
        self.ops.update(cx, |ops, cx| ops.request_paste(dest, cx));
    }

    /// 比较比哪两个目录。
    ///
    /// 双栏时就是屏幕上左右两栏（按位置，不是按"谁有焦点"）；
    /// 单栏时看选中了几个目录：两个就比这两个，一个就和当前目录比。
    /// 一个都挑不出来时返回 `None`，命令面板里也就不列这一条——
    /// 只有当前目录一个人，没得比。
    fn compare_targets(&self, cx: &App) -> Option<(PathBuf, PathBuf)> {
        if let Some(partner) = self.split {
            let (left_index, right_index) = if self.focus_on_left {
                (self.active, partner)
            } else {
                (partner, self.active)
            };
            let left = self.tabs[left_index].read(cx).path().to_path_buf();
            let right = self.tabs[right_index].read(cx).path().to_path_buf();
            if left != right {
                return Some((left, right));
            }
        }
        let pane = self.tabs[self.active].read(cx);
        let dirs: Vec<PathBuf> = pane
            .selected_paths()
            .into_iter()
            .filter(|path| path.is_dir())
            .collect();
        match dirs.len() {
            2 => Some((dirs[0].clone(), dirs[1].clone())),
            1 => Some((pane.path().to_path_buf(), dirs[0].clone())),
            _ => None,
        }
    }

    /// 并排比哪两个文件。
    ///
    /// 双栏时取左右两栏各一个（选中优先，没有选中就用光标下那个）；
    /// 单栏时取选中的前两个文件。挑不出来就不列这条命令。
    fn diff_targets(&self, cx: &App) -> Option<(PathBuf, PathBuf)> {
        if let Some(partner) = self.split {
            let (left_index, right_index) = if self.focus_on_left {
                (self.active, partner)
            } else {
                (partner, self.active)
            };
            let left = self.file_in_tab(left_index, cx);
            let right = self.file_in_tab(right_index, cx);
            match (left, right) {
                (Some(left), Some(right)) if left != right => return Some((left, right)),
                _ => {}
            }
        }
        let files: Vec<PathBuf> = self.tabs[self.active]
            .read(cx)
            .selected_paths()
            .into_iter()
            .filter(|path| path.is_file())
            .collect();
        if files.len() >= 2 {
            return Some((files[0].clone(), files[1].clone()));
        }
        None
    }

    /// 这一栏里"当前那一个文件"：选中优先，没有选中就用光标下的。
    fn file_in_tab(&self, index: usize, cx: &App) -> Option<PathBuf> {
        let pane = self.tabs[index].read(cx);
        pane.selected_paths()
            .into_iter()
            .find(|path| path.is_file())
    }

    /// 打开文件内容比较面板；已经开着就换掉（对象可能变了）。
    fn open_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((left, right)) = self.diff_targets(cx) else {
            return;
        };
        let panel = cx.new(|cx| DiffPanel::new(left, right, cx));
        self.subscriptions.push(cx.subscribe_in(
            &panel,
            window,
            |this, _, event: &DiffEvent, window, cx| match event {
                DiffEvent::Close => {
                    this.diff = None;
                    // 面板带着焦点离开渲染树，键盘得有人接手。
                    this.hand_keyboard_to_active_pane(window, cx);
                    cx.notify();
                }
                DiffEvent::Reveal(path) => this.reveal_in_active(path.clone(), window, cx),
            },
        ));
        let focus = panel.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.diff = Some(panel);
        cx.notify();
    }

    // ---- 远程连接 ----

    /// 后台回来的三件事：连上了、没连上、打开远程文件失败。
    fn on_remotes_event(
        &mut self,
        event: &RemotesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RemotesEvent::Connected { id, name } => {
                // 密码只进系统钥匙串，不写进配置文件——这句话要说出来，
                // 否则用户只能靠猜才知道密码去了哪。
                self.secret_note = Some(format!("「{name}」的密码已存入系统钥匙串"));
                self.connect = None;
                // 连上就跳过去：跳过去之后键盘也该在那一栏上（对话框里的
                // 输入框已经跟着关了）。
                self.hand_keyboard_to_active_pane(window, cx);
                self.open_remote(id.clone(), window, cx);
            }
            RemotesEvent::Failed(reason) => {
                // 对话框可能已经关了（比如用户等不及按了 Esc）：
                // 那就只在状态栏说一句，别把对话框又弹回来。
                if let Some(state) = self.connect.as_mut() {
                    state.busy = false;
                    state.error = Some(reason.clone());
                }
            }
            // 编辑远程文件：改完自动回传，这句话得说清楚——
            // 不然用户不知道自己动的是服务器上的原件。
            RemotesEvent::Editing { name } => {
                self.secret_note = Some(format!("「{name}」存一份在本地编辑，保存即回传"));
            }
            RemotesEvent::Uploaded { name, error } => {
                self.secret_note = Some(match error {
                    Some(reason) => format!("回传「{name}」失败：{reason}"),
                    None => format!("「{name}」已回传"),
                });
            }
            RemotesEvent::Listed { .. } => {}
        }
        cx.notify();
    }

    /// 把日志复制一份到桌面，状态栏报出路径。
    ///
    /// **落到桌面而不是让用户自己去缓存目录找**：日志文件一直在那儿写，
    /// 而用户既不知道路径在哪、也不该去那种地方翻。导出这一步就是
    /// "把程序里的现场变成一个能拖进聊天窗口的文件"。
    ///
    /// 桌面取不到（Linux 上没有 XDG 桌面目录、无头环境）就退回用户主目录：
    /// 拿不到时跟用户说"存到主目录了"，别只说失败——
    /// 那份日志他多半还是想要的。
    fn export_log(&mut self, cx: &mut Context<Self>) {
        // 记一笔"用户导出过"本身也有用：排查时能看出
        // 用户看到的问题发生在导出之前还是之后。
        crate::logfile::info("用户导出日志");
        // `path()` 顺带把「这次运行有没有日志文件」这件事判了：
        // 一次 `log()` 都还没发生的话它是 `None`，而此时报
        // "导出失败"是误导——没有东西可导，不是导不出来。
        if crate::logfile::path().is_none() {
            self.ops.update(cx, |ops, cx| {
                ops.say(
                    crate::i18n::Message::msg("还没有日志可导出：这次运行没记下任何东西"),
                    true,
                    cx,
                );
            });
            return;
        }
        let where_to = dirs::desktop_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        // 文件名带时间戳：用户导出一次、过一会儿再导一次，
        // 两份得能分清，否则第二份盖掉第一份而他还在拿旧那份说事。
        let name = format!("wfiles-{}.log", crate::format::stamp_for_file_name());
        let dest = where_to.join(name);
        match crate::logfile::export_to(&dest) {
            Ok(path) => self.ops.update(cx, |ops, cx| {
                ops.say(
                    crate::i18n::Message::args(
                        "日志已导出到 {path}",
                        &[("path", path.display().to_string())],
                    ),
                    false,
                    cx,
                );
            }),
            Err(error) => self.ops.update(cx, |ops, cx| {
                ops.say(
                    crate::i18n::Message::args(
                        "导出日志失败：{error}",
                        &[("error", error.to_string())],
                    ),
                    true,
                    cx,
                );
            }),
        }
    }

    /// 打开连接对话框。
    fn open_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connect.is_some() {
            return;
        }
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = field("坚果云", window, cx);
        let url = field(RemoteKind::WebDav.url_hint(), window, cx);
        let user = field("用户名（可留空）", window, cx);
        let password = field("密码或应用专用密码", window, cx);
        let region = field("us-east-1", window, cx);
        let key = field("~/.ssh/id_ed25519", window, cx);
        name.update(cx, |input, cx| input.focus(window, cx));
        self.connect = Some(ConnectState {
            kind: RemoteKind::WebDav,
            name,
            url,
            user,
            password,
            region,
            key,
            busy: false,
            error: None,
        });
        cx.notify();
    }

    fn close_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.connect = None;
        // 连接框里那几个输入框没了，键盘交给活动栏（见
        // `hand_keyboard_to_active_pane`）。
        self.hand_keyboard_to_active_pane(window, cx);
        cx.notify();
    }

    /// 连接对话框里地址那栏此刻填着什么（测试读状态用）。
    ///
    /// 界面上读不出来：没填的时候看到的是占位提示，而占位提示恰好也是
    /// 一个地址，光看界面分不清"填了"和"没填"。
    #[cfg(test)]
    pub(crate) fn connect_url(&self, cx: &App) -> String {
        self.connect
            .as_ref()
            .map(|state| state.url.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// 点"连接"：收齐四个框，交给 `Remotes` 去后台试连。
    ///
    /// 试连而不是直接存：地址打错一个字，存下来的是一个永远打不开的连接，
    /// 用户还得自己去找怎么删。
    fn submit_connect(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.connect.as_mut() else {
            return;
        };
        if state.busy {
            return;
        }
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let url = value(&state.url);
        let user = value(&state.user);
        let password = state.password.read(cx).value().to_string();
        let name = value(&state.name);

        if url.is_empty() {
            state.error = Some(crate::i18n::Message::msg("服务器地址不能为空"));
            cx.notify();
            return;
        }
        // 没填名字就从地址里取主机名，别让用户为了必填而编一个。
        let name = if name.is_empty() {
            host_of(&url).unwrap_or_else(|| "远程服务器".to_string())
        } else {
            name
        };
        let id = self
            .remotes
            .update(cx, |remotes, cx| remotes.fresh_id(&name, cx));
        let mut config = remote::Config::new(id, name, state.kind, url, user);
        if state.kind == RemoteKind::S3 {
            // 地区是签名的一部分：填错了会一直 403，
            // 而 403 看起来跟"密钥错了"一模一样，所以留空时按地址自己推并写进配置。
            let region = state.region.read(cx).value().trim().to_string();
            let region = if region.is_empty() {
                crate::s3::region_from_url(&config.url)
            } else {
                region
            };
            config = config.with_region(region);
        }
        if state.kind.has_key() {
            let key = state.key.read(cx).value().trim().to_string();
            if !key.is_empty() {
                config = config.with_key(key);
            }
        }

        state.busy = true;
        state.error = None;
        let secret = password;
        self.remotes
            .update(cx, |remotes, cx| remotes.connect(config, &secret, cx));
        cx.notify();
    }

    /// 打开一条存下来的连接：跳到它的根（`config.root` 可能是个子目录）。
    fn open_remote(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = Settings::peek(cx, |settings| {
            settings.remotes.iter().find(|it| it.id == id).cloned()
        }) else {
            return;
        };
        let path = remote::location(&config.id, &config.root);
        self.navigate_active_tab(path, window, cx);
    }

    /// 删掉一条连接：配置、钥匙串里的密码、目录缓存一起清掉。
    ///
    /// 侧栏上按第一次只是"举起手"，按第二次才真删——
    /// 这一步删不掉用户任何文件，但会删掉一个记不住的密码。
    fn forget_remote(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_armed = None;
        if self
            .secret_note
            .as_deref()
            .is_some_and(|note| note.contains(id))
        {
            self.secret_note = None;
        }
        self.remotes
            .update(cx, |remotes, cx| remotes.forget(id, cx));
        // 正停在那个远程目录里的标签要退回本地主页：
        // 留着会显示一个永远刷不出来的空列表。
        let home = Self::initial_dir();
        let stale: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                tab.read(cx).path().to_string_lossy().contains(&format!(
                    "{}://{}/",
                    remote::SCHEME,
                    id
                ))
            })
            .map(|(index, _)| index)
            .collect();
        for index in stale {
            let tab = self.tabs[index].clone();
            tab.update(cx, |pane, cx| pane.reset_to(home.clone(), window, cx));
        }
        cx.notify();
    }

    /// 换列表右边那一列元数据。
    ///
    /// 只改设置：值由 `Metadata` 自己去跟设置对（它每次被问起时都对一次），
    /// 这样"设置里改了"和"面板上点了"是同一条路，不会出现两处状态打架。
    fn set_meta_column(&mut self, field: Option<MetaField>, cx: &mut Context<Self>) {
        Settings::update(cx, |settings| {
            settings.meta_column = field.map_or_else(String::new, |field| field.id().to_string());
        });
        for tab in &self.tabs {
            tab.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// 打开比较面板，或者在开着的时候收起它。
    fn toggle_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compare.take().is_some() {
            // 面板开着时焦点在它身上，收起来得还键盘（见 `hand_keyboard_to_active_pane`）。
            self.hand_keyboard_to_active_pane(window, cx);
            cx.notify();
            return;
        }
        let Some((left, right)) = self.compare_targets(cx) else {
            return;
        };
        let ops = self.ops.clone();
        let panel =
            cx.new(|cx| ComparePanel::new(ops, CompareOptions::new(left, right), window, cx));
        self.subscriptions.push(cx.subscribe_in(
            &panel,
            window,
            |this, _, event: &CompareEvent, window, cx| match event {
                CompareEvent::Close => {
                    this.compare = None;
                    // 面板带着焦点离开渲染树，键盘得有人接手。
                    this.hand_keyboard_to_active_pane(window, cx);
                    cx.notify();
                }
                CompareEvent::Reveal(path) => this.reveal_in_active(path.clone(), window, cx),
            },
        ));
        // 面板这一帧才挂上窗口，焦点等挂好再给。
        let focus = panel.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.compare = Some(panel);
        cx.notify();
    }

    /// 跳到某个文件并收起比较面板：由面板的 Enter 触发。
    fn reveal_in_active(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.compare = None;
        self.tabs[self.active].update(cx, |pane, cx| pane.reveal_path(path, window, cx));
        cx.notify();
    }

    /// 开始听转发：抢到主实例位子的那一份，窗口开着就一直听着。
    ///
    /// 界面线程不做网络：绑端口、收连接、读请求全在后台线程上，只有
    /// "把请求落到界面上"这一小步经通道回来（同搬运那套）。
    pub fn serve_forwarding(
        &self,
        claim: instance::Claim,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = claim.listen() else {
            // 门牌挂不出去（端口绑不上、状态目录写不了）：这一次不做转发。
            // 窗口照开——转发是顺手带的功能，不是开窗的前提。
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(launch) = requests.recv().await {
                // 窗口关了就没得可办：这条任务到此为止，再收到也没人接。
                if this
                    .update_in(cx, |app, window, cx| {
                        app.apply_forwarded(launch, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// 第二份启动把"要开到哪儿"说到这儿来了。
    ///
    /// 落法跟命令行启动时一样（规则在 `cli::Target::resolve` 里，那边有
    /// 测试），只是这一次落在**已经开着**的窗口上：不碰恢复现场那一套，
    /// 直接挪当前那一栏，或者另开一栏。
    pub fn apply_forwarded(
        &mut self,
        launch: cli::Launch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opening = launch.target.resolve(Self::on_disk(launch.target.path()));
        // 记一句：主实例这一头收到了什么。跟 `instance::hand_off` 那行合起来
        // 就能看出"送出去没有、收到了没有"。
        crate::logfile::info(&format!(
            "转发：收到第二次启动的目标「{}」{}",
            launch.target.path().display(),
            if launch.new_tab {
                "（另开一栏）"
            } else {
                ""
            }
        ));
        // 用户是在别的程序里（终端、脚本、别的窗口的"用 WFiles 打开"）
        // 点的这一下：窗口该到前面来，不然他还得自己去找。系统文件管理器
        // 对这个动作都是这个反应。
        window.activate_window();
        // 用户要去别处了，比较面板留着只会碍事——它说的还是刚才那两栏的事。
        self.compare = None;
        if launch.new_tab {
            self.add_tab(opening.dir, window, cx);
        } else {
            self.navigate_active_tab(opening.dir, window, cx);
        }
        if let Some(path) = opening.select {
            // 刚换目录，列表已经就位（本地目录是同步读的）；里面真要没有
            // 这一项，`select_path` 自己就什么都不做。
            let pane = self.tabs[self.active].clone();
            pane.update(cx, |pane, cx| pane.select_path(&path, cx));
        }
    }

    /// 同步浏览：焦点那一栏换了目录，另一栏跟着走。
    ///
    /// 只从焦点栏往外传，另一栏的变化不会再传回来，所以不会两边互相推。
    fn mirror_navigation(
        &mut self,
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sync {
            return;
        }
        let Some(partner) = self.split else {
            return;
        };
        // 只从焦点栏往外传：另一栏的变化不会再传回来，两边不会互相推。
        if self.tabs.get(self.active) != Some(pane) {
            return;
        }
        let path = pane.read(cx).path().to_path_buf();
        let other = self.tabs[partner].clone();
        if other.read(cx).path() == path {
            return;
        }
        // 用 follow_to 而不是 navigate_to：这一栏是被带着走的，不该抢焦点。
        other.update(cx, |pane, cx| pane.follow_to(path, window, cx));
    }

    /// 某一栏被点了：点的是另一栏就把焦点挪过去，两栏位置不动。
    fn note_focus(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let Some(partner) = self.split else {
            return;
        };
        if self.tabs.get(partner) != Some(pane) {
            return;
        }
        self.focus_partner(cx);
    }

    // ---- 会话 ----

    fn snapshot(&self, cx: &Context<Self>) -> Session {
        Session {
            tabs: self
                .tabs
                .iter()
                .map(|tab| tab.read(cx).path().to_path_buf())
                .collect(),
            active: self.active,
            split: self.split,
            focus_on_left: self.focus_on_left,
            sync: self.sync,
        }
    }

    /// 有变化才写盘；写失败不影响使用（下次启动退回默认会话）。
    /// 窗格广播了一件事过来。
    ///
    /// 抽成共用处理器而不是在两处订阅里各写一遍 `match`：新开标签页
    /// （`open_tab`）与会话恢复（`app::init` 里那个循环）都要接，
    /// 漏一处的话那条事件就只在某一种来源下有反应——而那种 bug 要等到
    /// "恢复出来的标签上收藏栏不刷新"才看得出来。
    fn on_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_session(cx);
        match event {
            PaneEvent::Navigated => self.mirror_navigation(pane, window, cx),
            PaneEvent::Focused => self.note_focus(pane, cx),
        }
    }

    fn sync_session(&mut self, cx: &mut Context<Self>) {
        // 副窗不留痕：会话文件只有一份（`session.json`），两扇窗都写就是
        // 互相覆盖——重启后恢复成哪一份全看谁最后导航过。
        if !self.primary {
            return;
        }
        let snapshot = self.snapshot(cx);
        if snapshot == self.saved_session {
            return;
        }
        if let Err(error) = snapshot.save() {
            // 只在控制台留一句，界面上不打扰用户。
            eprintln!("保存会话失败：{error}");
        }
        self.saved_session = snapshot;
    }

    /// 把某一栏的恢复现场从盘上删掉（标签被关时用）。
    fn forget_restore(&self, dir: &std::path::Path) {
        // 副窗关标签不删别人的现场：那一条很可能是主窗正在用的
        // （两扇窗看着同一个目录是常态——副窗恢复的就是同一份会话）。
        if !self.primary {
            return;
        }
        let mut set = crate::restore::RestoreSet::load();
        set.forget(dir);
        if let Err(error) = set.save() {
            eprintln!("清理恢复现场失败：{error}");
        }
    }

    /// 启动时给恢复文件清一遍：剔掉已经关掉的标签和已经不在的文件。
    ///
    /// 跟 [`Self::forget_restore`] 一起用，不是替代：那边管"用户刚关的"，
    /// 这边兜住"上次崩的时候开着、这次没开"——比如崩之前那批标签对应的
    /// 目录这次没被恢复回来（会话文件也坏了一类的情形）。
    fn sweep_restore(&self, cx: &Context<Self>) {
        // 副窗不剪：它只开着目标那一栏（或者干脆在主页），剪一遍就等于把主窗
        // 正在看的那些目录的现场（选中项、光标）全删了。
        if !self.primary {
            return;
        }
        let live: Vec<PathBuf> = self
            .tabs
            .iter()
            .map(|tab| tab.read(cx).path().to_path_buf())
            .collect();
        let mut set = crate::restore::RestoreSet::load();
        let before = set.items.len();
        set.sanitized(&live);
        if set.items.len() == before {
            return;
        }
        if let Err(error) = set.save() {
            eprintln!("清理恢复现场失败：{error}");
        }
    }

    // ---- 文件操作事件 ----

    fn on_ops_event(&mut self, event: &OpsEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            OpsEvent::Changed {
                removed,
                changed,
                renamed_to,
            } => {
                // 文件动过了，Git 那一列的状态就得重来：
                // 复制、移动、删除都会让它变。
                self.meta.update(cx, |meta, _| meta.invalidate_git());
                for tab in &self.tabs {
                    tab.update(cx, |pane, cx| {
                        pane.apply_external_change(removed, changed, renamed_to, window, cx)
                    });
                }
            }
            OpsEvent::ClipboardChanged => {}
            OpsEvent::PromptChanged { open } => {
                if *open {
                    if self.prompt_input.is_none() {
                        let initial = self
                            .ops
                            .read(cx)
                            .prompt()
                            .map(|prompt| prompt.initial.clone())
                            .unwrap_or_default();
                        // 同 `open_palette`：闭包里的 `cx` 是新建实体的，
                        // 占位符的文案得在外面查好传进去。
                        let locale = Settings::read(cx).locale();
                        let input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .default_value(initial)
                                .placeholder(crate::i18n::tr_in(locale, "名称"))
                                .submit_on_enter(true)
                        });
                        input.update(cx, |input, cx| input.focus(window, cx));
                        self.subscriptions.push(cx.subscribe_in(
                            &input,
                            window,
                            |this, _, event: &InputEvent, _, cx| {
                                if let InputEvent::PressEnter { .. } = event {
                                    let value = this
                                        .prompt_input
                                        .as_ref()
                                        .map(|input| input.read(cx).value().to_string())
                                        .unwrap_or_default();
                                    this.ops.update(cx, |ops, cx| ops.submit_prompt(value, cx));
                                }
                                // 不在失焦时关闭：点“创建”按钮会让输入框先失焦，
                                // 那时关掉对话框按钮就再也点不到了。关闭只认
                                // 取消按钮和点遮罩。
                            },
                        ));
                        self.prompt_input = Some(input);
                    }
                } else {
                    self.prompt_input = None;
                    // 输入框一放下键盘就没了主人：得有人接手，不然应用级
                    // 快捷键会跟着哑掉（见 `hand_keyboard_to_active_pane`）。
                    // 用户刚才不是敲了回车就是按了取消，接下来该能直接使键盘。
                    self.hand_keyboard_to_active_pane(window, cx);
                }
            }
            OpsEvent::BulkChanged { open } => {
                if *open {
                    if self.bulk_inputs.is_none() {
                        self.create_bulk_inputs(window, cx);
                    }
                } else {
                    self.bulk_inputs = None;
                    self.hand_keyboard_to_active_pane(window, cx);
                }
            }
        }
    }

    /// 建批量重命名对话框的五个输入框，任何一个变动都重算整批预览。
    fn create_bulk_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rule = self
            .ops
            .read(cx)
            .bulk()
            .map(|bulk| bulk.rule.clone())
            .unwrap_or_default();

        let mut field = |initial: String,
                         placeholder: &'static str,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(initial)
                    .placeholder(placeholder)
            });
            self.subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    // 失焦/聚焦不关心；只有内容变了才重算。
                    if matches!(event, InputEvent::Change) {
                        this.sync_bulk_rule(cx);
                    }
                },
            ));
            input
        };

        let template = field(rule.template.clone(), "{name}{ext}", window, cx);
        let find = field(rule.find.clone(), "查找（可留空）", window, cx);
        let replace = field(rule.replace_with.clone(), "替换为（可留空）", window, cx);
        let start = field(rule.start.to_string(), "1", window, cx);
        let step = field(rule.step.to_string(), "1", window, cx);

        self.bulk_inputs = Some(BulkInputs {
            template,
            find,
            replace,
            start,
            step,
        });
        if let Some(inputs) = self.bulk_inputs.as_ref() {
            inputs
                .template
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// 把五个输入框的当前内容收成一套规则，交给 ops 重算预览。
    fn sync_bulk_rule(&mut self, cx: &mut Context<Self>) {
        let Some(inputs) = self.bulk_inputs.as_ref() else {
            return;
        };
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let rule = BulkRename {
            template: value(&inputs.template),
            find: value(&inputs.find),
            replace_with: value(&inputs.replace),
            // 序号解析不出来就退回默认值，输入过程中的中间态不该报错。
            start: value(&inputs.start).trim().parse().unwrap_or(1),
            step: value(&inputs.step).trim().parse().unwrap_or(1).max(1),
        };
        self.ops.update(cx, |ops, cx| ops.set_bulk_rule(rule, cx));
    }

    fn dismiss_bulk(&mut self, cx: &mut Context<Self>) {
        self.ops.update(cx, |ops, cx| ops.dismiss_bulk(cx));
        self.bulk_inputs = None;
    }

    // ---- 命令面板 ----

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
        } else {
            self.open_palette(window, cx);
        }
    }

    /// 面板当前列出的条目标题（面板没开就是空列表）。
    ///
    /// 给集成测试用：验"整屏一条中文都不剩"这种判据需要读全部条目，
    /// 而渲染树里没法按行遍历（行的 id 是命令 id，`.id()` 不叠加，
    /// 另加一个行号 id 会把命令 id 顶掉）。
    ///
    /// 返回**快照**而不是引用：调用方在 `App::update` 之外用它，
    /// 拿借用会跨不过那个边界。
    #[cfg(test)]
    pub fn palette_titles(&self) -> Vec<String> {
        self.palette
            .as_ref()
            .map(|state| {
                state
                    .items
                    .iter()
                    .map(|item| item.command.title.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 别的弹层开着就别再叠一层，先关掉。
        self.ops.update(cx, |ops, cx| ops.dismiss_prompt(cx));
        self.prompt_input = None;
        self.ops.update(cx, |ops, cx| ops.dismiss_bulk(cx));
        self.bulk_inputs = None;
        self.ops.update(cx, |ops, cx| ops.confirm_cancel(cx));
        self.ops.update(cx, |ops, cx| ops.cancel_conflict(cx));

        let items = self.build_palette_items(cx);
        let commands: Vec<PaletteCommand> = items.iter().map(|item| item.command.clone()).collect();
        self.palette = Some(PaletteState {
            items,
            filtered: palette::filter(&commands, ""),
            selected: 0,
        });

        // 占位符要查表，但 `cx.new` 闭包里那个 `cx` 是**新建实体**的上下文，
        // 不是外层窗口的——拿它 `tr()` 读到的是空设置，界面上一律回落中文。
        let locale = Settings::read(cx).locale();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::i18n::tr_in(locale, "搜索命令、位置或标签页"))
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        // 注意用闭包自己的 window 参数，不能捕获外层的：
        // subscribe_in 要求闭包 'static，捕获 &mut Window 会逃逸。
        self.subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.sync_palette_filter(cx),
                InputEvent::PressEnter { .. } => {
                    this.confirm_palette(window, cx);
                }
                _ => {}
            },
        ));
        self.palette_input = Some(input);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_input = None;
        // 面板自己拿着键盘，关掉之后得有人接手：那个输入框已经没了，
        // 键盘落在谁身上都不对——不接就是"应用级快捷键全哑"（见
        // `hand_keyboard_to_active_pane`）。
        self.hand_keyboard_to_active_pane(window, cx);
        cx.notify();
    }

    /// 查询词变了：重新过滤，选中项回到第一条。
    fn sync_palette_filter(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.palette_input.as_ref() else {
            return;
        };
        let query = input.read(cx).value().to_string();
        let Some(state) = self.palette.as_mut() else {
            return;
        };
        let commands: Vec<PaletteCommand> = state
            .items
            .iter()
            .map(|item| item.command.clone())
            .collect();
        state.filtered = palette::filter(&commands, &query);
        state.selected = 0;
        cx.notify();
    }

    fn move_palette_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(state) = self.palette.as_mut() else {
            return;
        };
        let count = state.filtered.len();
        if count == 0 {
            return;
        }
        let current = state.selected as i32;
        // 取模保证在 0..count 里循环，负 delta 也能正确回绕。
        let next = (current + delta).rem_euclid(count as i32);
        state.selected = next as usize;
        cx.notify();
    }

    fn confirm_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.palette.as_ref() else {
            return;
        };
        let Some(&item_index) = state.filtered.get(state.selected) else {
            return;
        };
        let action = state.items[item_index].action.clone();
        self.run_palette_action(action, window, cx);
    }

    /// 设一个搬运限速。正在跑的那一趟立刻按新的来，不用等它结束。
    fn set_speed_limit(&mut self, bytes_per_second: u64, cx: &mut Context<Self>) {
        Settings::update(cx, |settings| settings.speed_limit = bytes_per_second);
        self.ops
            .update(cx, |ops, cx| ops.set_speed_limit(bytes_per_second, cx));
        cx.notify();
    }

    fn run_palette_action(
        &mut self,
        action: PaletteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_palette(window, cx);
        match action {
            PaletteAction::GoTo(path) => self.navigate_active_tab(path, window, cx),
            PaletteAction::SwitchTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                    self.hand_keyboard_to_active_pane(window, cx);
                    self.sync_session(cx);
                    cx.notify();
                }
            }
            PaletteAction::Pane(command) => {
                self.tabs[self.active].update(cx, |pane, cx| pane.run_command(command, window, cx));
            }
            PaletteAction::EditKey(command) => {
                self.tabs[self.active]
                    .update(cx, |pane, cx| pane.start_capture(command, window, cx));
            }
            PaletteAction::SetTerminal(id) => {
                Settings::update(cx, |settings| settings.terminal = id.clone());
            }
            PaletteAction::MetaColumn(field) => self.set_meta_column(field, cx),
            PaletteAction::Connect => self.open_connect(window, cx),
            PaletteAction::OpenRemote(id) => self.open_remote(id, window, cx),
            PaletteAction::SetSpeedLimit(rate) => self.set_speed_limit(rate, cx),
            PaletteAction::SetLocale(locale) => {
                // 存设置 → 全部界面下一次重画就是新语言。
                // 不逐个 `cx.refresh()`：设置改了但没立刻重画的话，
                // 用户会看着半中半英的界面以为是自己按错了什么。
                Settings::update(cx, |settings| settings.set_locale(locale));
                cx.notify();
            }
            PaletteAction::SetLocaleAuto => {
                // 存空串而不是"把当前解析出来的语言写回去"：后者会让
                // "跟随系统"永远失效——用户换了系统语言也不生效，
                // 而界面上还写着"跟随系统"，那是在骗人。
                Settings::update(cx, |settings| settings.locale = String::new());
                cx.notify();
            }
            PaletteAction::ToggleTerminal => self.toggle_terminal(window, cx),
            PaletteAction::ExportLog => self.export_log(cx),
            PaletteAction::SavedSearch(name) => {
                let Some(saved) = Settings::read(cx).saved_search(&name).cloned() else {
                    // 存过又被删掉了：命令面板是打开时快照的，这一条已经过期。
                    return;
                };
                self.tabs[self.active]
                    .update(cx, |pane, cx| pane.run_saved_search(&saved, window, cx));
            }
            PaletteAction::NewTab => self.new_tab(window, cx),
            PaletteAction::CloseTab => self.close_tab(self.active, window, cx),
            PaletteAction::NextTab => self.next_tab(window, cx),
            PaletteAction::App(command) => match command {
                AppCommand::ToggleSplit => self.toggle_split(window, cx),
                AppCommand::SwapPane => self.focus_other(window, cx),
                AppCommand::ToggleSync => self.toggle_sync(window, cx),
                AppCommand::CopyToOther => self.transfer_to_other(false, cx),
                AppCommand::MoveToOther => self.transfer_to_other(true, cx),
                AppCommand::ToggleSidebar => self.toggle_sidebar(cx),
                AppCommand::ToggleDetails => self.toggle_details(cx),
                AppCommand::Compare => self.toggle_compare(window, cx),
                AppCommand::Diff => self.open_diff(window, cx),
            },
        }
    }

    /// 当前这一刻能用的命令。选不中的、剪贴板空的、没得可撤的都不列，
    /// 免得面板里躺着一堆按了没反应的项。
    fn build_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let mut items = Vec::new();

        for command in [
            PaneCommand::NewFolder,
            PaneCommand::Rename,
            PaneCommand::BulkRename,
            PaneCommand::Trash,
            PaneCommand::Copy,
            PaneCommand::Cut,
            PaneCommand::Paste,
            PaneCommand::Undo,
            PaneCommand::Refresh,
            PaneCommand::GoUp,
            PaneCommand::CopyPath,
            PaneCommand::CopyFileName,
            PaneCommand::OpenTerminal,
            PaneCommand::QuickLook,
            PaneCommand::Search,
            PaneCommand::Mark,
            PaneCommand::ViewArchive,
            PaneCommand::CreateArchive,
            PaneCommand::EditRemote,
            PaneCommand::ViewList,
            PaneCommand::ViewGrid,
            PaneCommand::ViewColumns,
            PaneCommand::Analyze,
            PaneCommand::ToggleHidden,
        ] {
            if !self.pane_command_enabled(command, cx) {
                continue;
            }
            let (title, hint, group, keywords) = Self::describe_pane_command(command, cx);
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("pane:{command:?}"),
                    title,
                    hint,
                    group,
                    keywords,
                ),
                action: PaletteAction::Pane(command),
            });
        }

        for saved in Settings::read(cx).saved_searches.iter() {
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("saved:{}", saved.name),
                    saved.name.clone(),
                    saved.query.clone(),
                    crate::i18n::tr(cx, "保存的搜索"),
                    format!("saved 保存的搜索 智能文件夹 {}", saved.query),
                ),
                action: PaletteAction::SavedSearch(saved.name.clone()),
            });
        }

        let mut seen_paths: Vec<PathBuf> = Vec::new();
        for (label, _, path) in Self::places() {
            // 极端情况下两个位置可能指向同一个目录，跳过重复的，
            // 免得元素 id 撞车。
            if seen_paths.contains(&path) {
                continue;
            }
            seen_paths.push(path.clone());
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("go:{}", path.display()),
                    // `label` 也要翻：侧栏已经翻了，这里是同一条路径的另一出口。
                    // 忘了翻的话侧栏写 "Home"、面板写 "个人文件夹"，
                    // 同一个地方两个名字。
                    crate::i18n::tr_args(
                        cx,
                        "前往{label}",
                        &[("label", crate::i18n::tr(cx, label))],
                    ),
                    String::new(),
                    crate::i18n::tr(cx, "前往"),
                    "go 位置 打开 跳转",
                ),
                action: PaletteAction::GoTo(path),
            });
        }

        for (index, tab) in self.tabs.iter().enumerate() {
            let path = tab.read(cx).path();
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let hint = if index == self.active {
                crate::i18n::tr(cx, "当前")
            } else {
                String::new()
            };
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("tab:{index}"),
                    crate::i18n::tr_args(cx, "切换到标签页：{label}", &[("label", label.clone())]),
                    hint,
                    crate::i18n::tr(cx, "标签页"),
                    "tab 切换 窗口",
                ),
                action: PaletteAction::SwitchTab(index),
            });
        }

        // 比较：比不出两个目录就不列，免得面板里躺着一条按了没反应的。
        if self.compare_targets(cx).is_some() {
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:compare",
                    crate::i18n::tr(cx, "比较这两个文件夹"),
                    "Cmd+Alt+B",
                    crate::i18n::tr(cx, "文件"),
                    "compare diff 比较 差异 同步 sync 两栏",
                ),
                action: PaletteAction::App(AppCommand::Compare),
            });
        }

        if self.diff_targets(cx).is_some() {
            let (left, right) = self.diff_targets(cx).expect("刚查过");
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:diff",
                    crate::i18n::tr_args(
                        cx,
                        "比较文件内容：{left} ↔ {right}",
                        &[("left", name_of(&left)), ("right", name_of(&right))],
                    ),
                    String::new(),
                    crate::i18n::tr(cx, "文件"),
                    "diff 比较 内容 差异 行 十六进制 hex 字节",
                ),
                action: PaletteAction::App(AppCommand::Diff),
            });
        }

        // ---- 元数据列 ----
        // 六种共用一列，所以这里是"选一个"，不是六个开关：
        // 全铺开会挤掉文件名，而一次通常只关心其中一种。
        let shown = Settings::peek(cx, |settings| MetaField::from_id(&settings.meta_column));
        for field in MetaField::ALL {
            if shown == Some(field) {
                continue;
            }
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("app:meta-{}", field.id()),
                    crate::i18n::tr_args(cx, "元数据列：{label}", &[("label", field.label(cx))]),
                    String::new(),
                    crate::i18n::tr(cx, "元数据列"),
                    field.keywords(),
                ),
                action: PaletteAction::MetaColumn(Some(field)),
            });
        }
        if shown.is_some() {
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:meta-off",
                    crate::i18n::tr(cx, "不显示元数据列"),
                    String::new(),
                    crate::i18n::tr(cx, "元数据列"),
                    "关掉 关闭 隐藏 不要 元数据列",
                ),
                action: PaletteAction::MetaColumn(None),
            });
        }

        // ---- 远程 ----
        // 存下来的连接一条一项，能直接跳过去；"连接服务器"放在最后，
        // 因为新建远没有打开常用。
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:connect",
                crate::i18n::tr(cx, "连接远程服务器…"),
                String::new(),
                crate::i18n::tr(cx, "远程"),
                "remote 远程 连接 webdav 服务器 坚果云 nextcloud 网盘 nas 挂载",
            ),
            action: PaletteAction::Connect,
        });
        for config in Settings::peek(cx, |settings| settings.remotes.clone()) {
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("remote:{}", config.id),
                    format!("打开远程：{}", config.name),
                    config.kind.label(),
                    "远程",
                    format!("remote 远程 打开 {} {}", config.name, config.url),
                ),
                action: PaletteAction::OpenRemote(config.id),
            });
        }

        // ---- 双栏 ----
        let split = self.split.is_some();
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:split",
                if split {
                    crate::i18n::tr(cx, "取消分栏")
                } else {
                    crate::i18n::tr(cx, "分栏显示")
                },
                "Cmd+Alt+D",
                crate::i18n::tr(cx, "窗口"),
                "split 双栏 分屏 两栏 pane",
            ),
            action: PaletteAction::App(AppCommand::ToggleSplit),
        });
        if split {
            let selected = self.tabs[self.active].read(cx).selection_len();
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:swap-pane",
                    crate::i18n::tr(cx, "切换到另一栏"),
                    "Tab",
                    crate::i18n::tr(cx, "窗口"),
                    "focus 焦点 切换 另一栏",
                ),
                action: PaletteAction::App(AppCommand::SwapPane),
            });
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:sync",
                    if self.sync {
                        crate::i18n::tr(cx, "关闭双栏同步浏览")
                    } else {
                        crate::i18n::tr(cx, "双栏同步浏览")
                    },
                    "Cmd+Alt+S",
                    crate::i18n::tr(cx, "窗口"),
                    "sync 同步 联动 导航",
                ),
                action: PaletteAction::App(AppCommand::ToggleSync),
            });
            if selected >= 1 {
                let partner = self
                    .split
                    .and_then(|index| self.tabs.get(index))
                    .map(|pane| pane.read(cx).path())
                    .map(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    })
                    .unwrap_or_default();
                items.push(PaletteItem {
                    command: PaletteCommand::new(
                        "app:copy-other",
                        crate::i18n::tr_args(
                            cx,
                            "拷贝到另一栏：{partner}",
                            &[("partner", partner.clone())],
                        ),
                        "F5",
                        crate::i18n::tr(cx, "文件"),
                        "copy 复制 另一栏 目标",
                    ),
                    action: PaletteAction::App(AppCommand::CopyToOther),
                });
                items.push(PaletteItem {
                    command: PaletteCommand::new(
                        "app:move-other",
                        crate::i18n::tr_args(
                            cx,
                            "移动到另一栏：{partner}",
                            &[("partner", partner.clone())],
                        ),
                        "F6",
                        crate::i18n::tr(cx, "文件"),
                        "move 移动 剪切 另一栏 目标",
                    ),
                    action: PaletteAction::App(AppCommand::MoveToOther),
                });
            }
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:toggle-sidebar",
                    if self.sidebar_collapsed {
                        crate::i18n::tr(cx, "展开侧栏")
                    } else {
                        crate::i18n::tr(cx, "收起侧栏")
                    },
                    "Cmd+B",
                    crate::i18n::tr(cx, "窗口"),
                    "sidebar 侧栏 收起 展开 折叠 窄",
                ),
                action: PaletteAction::App(AppCommand::ToggleSidebar),
            });
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    "app:toggle-details",
                    if self.details_shown_in_active(cx) {
                        crate::i18n::tr(cx, "收起详情板")
                    } else {
                        crate::i18n::tr(cx, "展开详情板")
                    },
                    "Cmd+Alt+O",
                    crate::i18n::tr(cx, "窗口"),
                    "details 详情 预览 信息 收起 展开 检查器",
                ),
                action: PaletteAction::App(AppCommand::ToggleDetails),
            });
        }

        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:new-tab",
                crate::i18n::tr(cx, "新建标签页"),
                "Cmd+T",
                crate::i18n::tr(cx, "标签页"),
                "tab 新开",
            ),
            action: PaletteAction::NewTab,
        });
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:close-tab",
                crate::i18n::tr(cx, "关闭标签页"),
                "Cmd+W",
                crate::i18n::tr(cx, "标签页"),
                "tab 关闭",
            ),
            action: PaletteAction::CloseTab,
        });
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:next-tab",
                crate::i18n::tr(cx, "下一个标签页"),
                "Ctrl+Tab",
                crate::i18n::tr(cx, "标签页"),
                "tab 切换",
            ),
            action: PaletteAction::NextTab,
        });

        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:toggle-terminal",
                if self.term.is_some() {
                    crate::i18n::tr(cx, "收起终端面板")
                } else {
                    crate::i18n::tr(cx, "打开终端面板")
                },
                "Ctrl+Alt+T",
                crate::i18n::tr(cx, "终端"),
                "terminal 内嵌 面板 shell 命令行 抽屉",
            ),
            action: PaletteAction::ToggleTerminal,
        });

        // 导日志没绑快捷键：它是低频动作（出了事才用），
        // 而快捷键表是要抢的——占一个键位不值，见 keymap.rs 的占用表。
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:export-log",
                crate::i18n::tr(cx, "导出日志到桌面"),
                "",
                crate::i18n::tr(cx, "排查问题"),
                "log 日志 导出 排查 问题 崩溃 crash 现场 logfile",
            ),
            action: PaletteAction::ExportLog,
        });

        // 换默认终端：只列装了的，免得选一个按下去报错的。
        let current = Settings::read(cx).terminal;
        items.push(PaletteItem {
            command: PaletteCommand::new(
                format!("terminal:{}", terminal::AUTO),
                crate::i18n::tr(cx, "默认终端：跟随系统"),
                if current == terminal::AUTO {
                    crate::i18n::tr(cx, "当前")
                } else {
                    String::new()
                },
                crate::i18n::tr(cx, "终端"),
                "terminal 默认 自动 shell",
            ),
            action: PaletteAction::SetTerminal(terminal::AUTO.to_string()),
        });
        for option in terminal::candidates() {
            if !terminal::installed(&option) {
                continue;
            }
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("terminal:{}", option.id),
                    crate::i18n::tr_args(
                        cx,
                        "默认终端：{name}",
                        &[("name", option.name.to_string())],
                    ),
                    if current == option.id {
                        crate::i18n::tr(cx, "当前")
                    } else {
                        String::new()
                    },
                    crate::i18n::tr(cx, "终端"),
                    "terminal shell 命令行",
                ),
                action: PaletteAction::SetTerminal(option.id.to_string()),
            });
        }

        // 限速：一次大拷贝会把整块磁盘吃掉，同一时间想干点别的就全卡住。
        let limit = Settings::read(cx).speed_limit;
        for rate in crate::throttle::rates() {
            let label = crate::throttle::rate_label(rate, cx);
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("speed:{rate}"),
                    crate::i18n::tr_args(cx, "传输限速：{label}", &[("label", label.to_string())]),
                    if rate == limit {
                        crate::i18n::tr(cx, "当前")
                    } else {
                        String::new()
                    },
                    crate::i18n::tr(cx, "传输"),
                    "speed limit 限速 带宽 传输 拷贝 慢一点",
                ),
                action: PaletteAction::SetSpeedLimit(rate),
            });
        }

        // 界面语言。列出来的名字**用该语言自己写**，跟系统语言选择器一个规矩：
        // 英文界面里那条写 "English"，用户在找英文时一眼认得出来；
        // 中文界面里同一条写 "简体中文"，也不需要任何解释。
        //
        // "跟随系统"是单独一个动作（`SetLocaleAuto`），不是"把当前语言再设一遍"：
        // 设置里存空串才是真的回到自动，把语言代码写回去是"就停在英文"，
        // 哪怕此刻显示的就是 "Current"。这两件事搅在一起的话，
        // 用户点了跟没点一样，下次装个新语言包也还是英文。
        let stored = Settings::peek(cx, |settings| settings.locale.clone());
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "locale:auto",
                crate::i18n::tr(cx, "界面语言：跟随系统"),
                if stored.is_empty() {
                    crate::i18n::tr(cx, "当前")
                } else {
                    String::new()
                },
                crate::i18n::tr(cx, "视图"),
                "language 语言 界面 跟随系统 system locale i18n",
            ),
            action: PaletteAction::SetLocaleAuto,
        });
        for option in crate::i18n::Locale::ALL {
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("locale:{}", option.code()),
                    crate::i18n::tr_args(
                        cx,
                        "界面语言：{label}",
                        &[("label", option.label().to_string())],
                    ),
                    if stored == option.code() {
                        crate::i18n::tr(cx, "当前")
                    } else {
                        String::new()
                    },
                    crate::i18n::tr(cx, "视图"),
                    "language 语言 界面 locale i18n english 中文",
                ),
                action: PaletteAction::SetLocale(*option),
            });
        }

        self.push_key_binding_items(&mut items, cx);

        items
    }

    /// 这条命令此刻按下去有没有效果。
    fn pane_command_enabled(&self, command: PaneCommand, cx: &Context<Self>) -> bool {
        let pane = self.tabs[self.active].read(cx);
        let selected = pane.selection_len();
        match command {
            PaneCommand::Rename => selected == 1,
            PaneCommand::BulkRename => selected >= 2,
            PaneCommand::Trash
            | PaneCommand::Copy
            | PaneCommand::Cut
            | PaneCommand::CopyPath
            | PaneCommand::CopyFileName => selected >= 1,
            PaneCommand::Paste => !self.ops.read(cx).clipboard().paths.is_empty(),
            PaneCommand::Undo => self.ops.read(cx).undo_label().is_some(),
            PaneCommand::GoUp => pane.has_parent(),
            // 没有选中任何东西时可没什么可"快速查看"的。
            PaneCommand::QuickLook => pane.quick_look_enabled(),
            // 光标得正指着一个 zip 才谈得上"看里面"；打包则要求有选中项。
            PaneCommand::ViewArchive => pane.archive_enabled(),
            PaneCommand::CreateArchive => selected >= 1,
            // 选中一个远程文件才谈得上"编辑并回传"。
            PaneCommand::EditRemote => pane.edit_remote_enabled(),
            // 标记是对一个文件做的事，多选时说不清该标谁。
            PaneCommand::Mark => selected == 1,
            // 收藏同理：多选时"收藏"还是"取消收藏"取决于其中有几项已经
            // 在里面，同一个键做两件相反的事说不清。
            PaneCommand::ToggleFavorite => pane.favorite_enabled(),
            _ => true,
        }
    }

    /// (标题, 快捷键提示, 分组, 搜索关键词)
    /// 命令面板里的"改键"分组：一条命令一项，点进去按个键就改好了。
    fn push_key_binding_items(&self, items: &mut Vec<PaletteItem>, cx: &Context<Self>) {
        let map = crate::keymap::KeyMap::read(cx);
        for command in PaneCommand::ALL {
            let (title, ..) = Self::describe_pane_command(command, cx);
            let key = map.key_for(command);
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("key:{}", command.id()),
                    // 带名字的占位符而不是 `{title}` 位置占位：英文的
                    // "Rebind: Rename" 和"改键：重命名"词序不同，位置占位换不了。
                    crate::i18n::tr_args(cx, "改键：{title}", &[("title", title.clone())]),
                    key.clone(),
                    crate::i18n::tr(cx, "快捷键"),
                    // 关键词里带上"快捷键"这一类词，搜"改键"就能把整组拉出来。
                    command.keywords(),
                ),
                action: PaletteAction::EditKey(command),
            });
        }
    }

    /// 一条命令在面板里长什么样：标题、快捷键、分组、面板自己的搜索词。
    ///
    /// **前两项要按当前语言查表**，所以这里收 `cx` 并返回 `String`，
    /// 而不是一个 `&'static str` 常量表——静态表没法翻，语言切换就只对
    /// 已经写成 `tr()` 的那些调用点生效，面板会变成"半翻译"。
    ///
    /// **第四项（搜索词）不翻**，这是有意的：它是给用户搜的东西，不是显示给人看的。
    /// 英文用户搜 "delete"、中文用户搜 "删除"，两边都得能搜到同一条命令——
    /// 翻了就有一半人搜不到自己那条命令了。表里那些中英混排的词正是这个用意。
    ///
    /// 快捷键（第二项）也不翻：`Cmd+Shift+N` 这种东西全平台一个样。
    fn describe_pane_command(
        command: PaneCommand,
        cx: &gpui_kit::App,
    ) -> (String, String, String, &'static str) {
        // 分组只有三个，两个字的中文词，翻成英文占更多地方。
        let files = crate::i18n::tr(cx, "文件");
        let view = crate::i18n::tr(cx, "视图");
        match command {
            PaneCommand::NewFolder => (
                crate::i18n::tr(cx, "新建文件夹"),
                Self::shortcut_of(command, cx),
                files,
                "folder 创建 目录",
            ),
            PaneCommand::Rename => (
                crate::i18n::tr(cx, "重命名"),
                Self::shortcut_of(command, cx),
                files,
                "rename 改名",
            ),
            PaneCommand::BulkRename => (
                crate::i18n::tr(cx, "批量重命名"),
                Self::shortcut_of(command, cx),
                files,
                "batch 序号 模板",
            ),
            PaneCommand::Trash => (
                crate::i18n::tr(cx, "移到回收站"),
                Self::shortcut_of(command, cx),
                files,
                "delete 删除 trash",
            ),
            PaneCommand::Copy => (
                crate::i18n::tr(cx, "拷贝选中项"),
                Self::shortcut_of(command, cx),
                files,
                "copy",
            ),
            PaneCommand::Cut => (
                crate::i18n::tr(cx, "剪切选中项"),
                Self::shortcut_of(command, cx),
                files,
                "cut 移动",
            ),
            PaneCommand::Paste => (
                crate::i18n::tr(cx, "粘贴到当前目录"),
                Self::shortcut_of(command, cx),
                files,
                "paste",
            ),
            PaneCommand::Undo => (
                crate::i18n::tr(cx, "撤销上一步操作"),
                Self::shortcut_of(command, cx),
                files,
                "undo 恢复",
            ),
            PaneCommand::Refresh => (
                crate::i18n::tr(cx, "刷新当前目录"),
                Self::shortcut_of(command, cx),
                files,
                "refresh 重新读取",
            ),
            PaneCommand::GoUp => (
                crate::i18n::tr(cx, "前往上一级"),
                Self::shortcut_of(command, cx),
                files,
                "up parent 上级",
            ),
            PaneCommand::CopyPath => (
                crate::i18n::tr(cx, "复制路径"),
                Self::shortcut_of(command, cx),
                files,
                "path 剪贴板",
            ),
            PaneCommand::CopyFileName => (
                crate::i18n::tr(cx, "复制文件名"),
                Self::shortcut_of(command, cx),
                files,
                "name 剪贴板",
            ),
            PaneCommand::OpenTerminal => (
                crate::i18n::tr(cx, "在这里打开终端"),
                Self::shortcut_of(command, cx),
                files,
                "terminal 终端 shell 命令行",
            ),
            PaneCommand::QuickLook => (
                crate::i18n::tr(cx, "快速查看"),
                Self::shortcut_of(command, cx),
                files,
                "preview 预览 quick look 查看",
            ),
            PaneCommand::Search => (
                crate::i18n::tr(cx, "搜索当前目录"),
                Self::shortcut_of(command, cx),
                files,
                "search 搜索 查找 find 内容 content grep",
            ),
            PaneCommand::Mark => (
                crate::i18n::tr(cx, "加标签 / 标颜色"),
                Self::shortcut_of(command, cx),
                files,
                "mark 标记 标签 颜色 tag color 色标",
            ),
            PaneCommand::ViewArchive => (
                crate::i18n::tr(cx, "查看压缩包内容"),
                Self::shortcut_of(command, cx),
                files,
                "zip 压缩包 archive 解压 unzip 浏览",
            ),
            PaneCommand::CreateArchive => (
                crate::i18n::tr(cx, "打包成 Zip"),
                Self::shortcut_of(command, cx),
                files,
                "zip 压缩包 archive 压缩 打包 compress",
            ),
            PaneCommand::EditRemote => (
                crate::i18n::tr(cx, "编辑远程文件"),
                Self::shortcut_of(command, cx),
                files,
                "edit 编辑 remote 远程 回传 上传 upload",
            ),
            // 换视图不作用于文件，归到"视图"组里更好找。
            PaneCommand::ViewList => (
                crate::i18n::tr(cx, "列表视图"),
                Self::shortcut_of(command, cx),
                view,
                "view list 列表 一行一项",
            ),
            PaneCommand::ViewGrid => (
                crate::i18n::tr(cx, "网格视图"),
                Self::shortcut_of(command, cx),
                view,
                "view grid 网格 缩略图",
            ),
            PaneCommand::ViewColumns => (
                crate::i18n::tr(cx, "分列视图"),
                Self::shortcut_of(command, cx),
                view,
                "view miller 分列 层级 columns",
            ),
            PaneCommand::Analyze => (
                crate::i18n::tr(cx, "分析：大小 / 重复 / 校验和"),
                Self::shortcut_of(command, cx),
                files,
                "analyze 分析 大小 重复 校验和 md5 sha1 sha256 size duplicate checksum",
            ),
            // 面板里给的是**中性说法**（不是"按下之后会变成什么"）：
            // 面板条目走的是 `palette_titles` 那条纯函数路径，拿不到
            // 窗格的 `listing`，报不出当前状态。工具栏按钮上才是
            // `Pane::hidden_switch_title` 那一版。
            PaneCommand::ToggleHidden => (
                crate::i18n::tr(cx, "显示 / 隐藏隐藏项"),
                Self::shortcut_of(command, cx),
                view,
                "hidden dotfile 点文件 隐藏 显示 dot 隐式",
            ),
            // 同样是中性说法：面板这一条走纯函数路径，能拿到选中项
            // （`favorite_enabled` 在下面单独判），但拿不到"当前这一项
            // 收藏了没有"——那要让用户自己在标题里猜，反而不如写中性。
            PaneCommand::ToggleFavorite => (
                crate::i18n::tr(cx, "收藏 / 取消收藏"),
                Self::shortcut_of(command, cx),
                files,
                "favorite 收藏 书签 bookmark 常用 侧栏",
            ),
        }
    }

    /// 一条命令在面板上显示的快捷键。
    ///
    /// **从 [`crate::keymap::defaults`] 取，不手写字面量**：
    /// 原来是 22 个手写串，加一条命令就得再抄一个——抄错不报编译错误，
    /// 只是面板上写着一个按不出来的键。已经栽过：
    /// 「编辑远程文件」那里写的是 `Cmd+Alt+O`（那是详情板的键），
    /// 而实际绑的是 `cmd-alt-e`。
    ///
    /// `cmd-shift-.` 这种写法显示成 `Cmd+Shift+.`（去掉 `cmd-` 前缀、
    /// 首字母大写），而 `space` / `f2` 显示成中文「空格」「F2」——
    /// 后者跟面板里其他条目的写法一致。
    fn shortcut_of(command: PaneCommand, cx: &gpui_kit::App) -> String {
        let key = crate::keymap::KeyMap::read(cx).key_for(command);
        if key.is_empty() {
            return String::new();
        }
        if key == "space" {
            // 空格在面板里一直写中文（跟"取消""完成"那些同一条街上），
            // 而这个函数收得到 `cx` 却**故意不查表**：面板里其余地方
            // 已经把快捷键排除在翻译之外了（见上面的注释），
            // 这里翻了就只有这一条是英文。
            return crate::i18n::tr(cx, "空格");
        }
        let pretty = key
            .strip_prefix("cmd-")
            .unwrap_or(key.as_str())
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join("+");
        // `cmd-` 前缀全平台一样（`super` / `control` 是同一层意思），
        // 所以显示成 `Cmd+…` 而不是按平台改写。
        format!("Cmd+{pretty}")
    }

    /// 命令面板：顶部居中的搜索框 + 分组结果列表。
    ///
    /// 键盘走卡片自己的 `on_key_down`（冒泡阶段），不依赖快捷键表——
    /// 输入框占着焦点，靠 keymap 的上下文优先级不可靠，
    /// 而元素级回调一定会在输入框之后收到。
    fn render_palette(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.palette.as_ref() else {
            return div().into_any_element();
        };
        let Some(input) = self.palette_input.clone() else {
            return div().into_any_element();
        };

        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;
        let selected_bg = cx.theme().muted;

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut current_group: Option<&str> = None;
        for (row_index, &item_index) in state.filtered.iter().enumerate() {
            let item = &state.items[item_index];
            if current_group != Some(item.command.group.as_str()) {
                current_group = Some(item.command.group.as_str());
                rows.push(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_xs()
                        .text_color(muted)
                        .child(item.command.group.clone())
                        .into_any_element(),
                );
            }
            let is_selected = row_index == state.selected;
            let action = item.action.clone();
            // 元素 id 直接用命令 id：界面调试和集成测试都能按命令定位，
            // 不用去数第几行。
            rows.push(
                h_flex()
                    // 命令 id 是**唯一**的 id：面板里不能有两个元素同一个 id，
                    // 而 `.id()` 不叠加（后写的覆盖先写的）——所以行号观察点
                    // 不能另加一个 `.id()`，那会把命令 id 顶掉，
                    // `try_find("pane:Refresh")` 这类定位就全废了。
                    //
                    // 要按行遍历靠 `.test_support()` 的路径数组，见 ui_tests
                    // 里的 `palette_titles`：它扫 `registered_paths()`。
                    .id(item.command.id.clone())
                    // test_support 是 GPUI 的透明观察包装（组件库自己也这么用），
                    // 让集成测试能按元素 id 定位；aria_label 顺带给读屏软件
                    // 一个完整的行描述，而不是把标题和快捷键拼在一起。
                    .test_support()
                    .aria_label(item.command.title.clone())
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded(radius)
                    .cursor_pointer()
                    .bg(if is_selected {
                        selected_bg
                    } else {
                        transparent_white()
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(item.command.title.clone()),
                    )
                    .when(!item.command.hint.is_empty(), |this| {
                        this.child(
                            div()
                                // hint 是"当前"这类状态标记，测试要能按命令 id 读到它
                                // （行的 aria_label 只有标题）。
                                //
                                // id 是拼出来的字符串而不是 `("palette-hint", command_id)`
                                // 元组：元组 id 的第二部分只认 u32 / usize / EntityId，
                                // **不认 String**，而命令 id 是字符串。
                                //
                                // hint 是**按需插入**的：没标记的行压根没这个元素，
                                // 所以"没标记"= 查不到，而不是查到一个空串。
                                .id(format!("palette-hint:{}", item.command.id))
                                .test_support()
                                .aria_label(item.command.hint.clone())
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(muted)
                                .child(item.command.hint.clone()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_palette_action(action.clone(), window, cx)
                    }))
                    .into_any_element(),
            );
        }

        let body: AnyElement = if rows.is_empty() {
            div()
                .py_6()
                .text_sm()
                .text_color(muted)
                // 读屏软件得知道"搜到0 条"，不然用户只听见一片静默。
                .id("palette-empty")
                .test_support()
                .aria_label(crate::i18n::tr(cx, "没有匹配的命令"))
                .child(crate::i18n::tr(cx, "没有匹配的命令"))
                .into_any_element()
        } else {
            // overflow_y_scroll 属于 StatefulInteractiveElement，
            // 只有带 id 的 Stateful<Div> 才有，所以先落一个 id。
            let list = div()
                .id("palette-list")
                .test_support()
                .flex()
                .flex_col()
                .max_h(px(380.))
                .overflow_y_scroll()
                .p_1()
                .children(rows);
            list.into_any_element()
        };

        let card = v_flex()
            .id("palette-card")
            .test_support()
            .w(px(600.))
            // 跟对话框同一道约束：600 px 是常用宽度，但窗口可以是 520 px。
            // 实测不加这句时卡片 x = -39、右边缘 559，两边各溢出几十像素，
            // "Esc 关闭"那行提示条直接看不见。
            .max_w_full()
            .max_h_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(46.))
                    .border_b_1()
                    .border_color(border)
                    .child(Icon::new(IconName::Search).size_4().text_color(muted))
                    .child(Input::new(&input).cleanable(true).w_full()),
            )
            // 列表自己滚：命令很多时它让出高度，输入框与底部提示条留在原地。
            .child(div().flex_1().min_h_0().child(body))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(px(30.))
                    .px_3()
                    .gap_3()
                    .items_center()
                    .border_t_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr(cx, "↑↓ 选择"))
                    .child(crate::i18n::tr(cx, "Enter 执行"))
                    .child(crate::i18n::tr(cx, "Esc 关闭")),
            );

        div()
            .id("palette-overlay")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            // 顶部 90 px 是"命令面板从上方落下来"的手感，不是安全边距——
            // 它得跟内容高度一起被窗口高度管住，所以改成"最多下探 90 px"：
            // 窗口够高时在 90 px 处，窗口不够高时贴着上边。
            .pt(px(90.))
            .px_4()
            .max_h_full()
            .bg(cx.theme().overlay)
            .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
            .child(card)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "up" => this.move_palette_selection(-1, cx),
                    "down" => this.move_palette_selection(1, cx),
                    "enter" => this.confirm_palette(window, cx),
                    "escape" => this.close_palette(window, cx),
                    _ => {}
                }
            }))
            .into_any_element()
    }

    fn dismiss_prompt(&mut self, cx: &mut Context<Self>) {
        self.ops.update(cx, |ops, cx| ops.dismiss_prompt(cx));
        self.prompt_input = None;
    }

    // ---- 渲染 ----

    /// 中间那块：单栏放一个窗格，双栏放两个加一条分隔线。
    fn render_panes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let visible = self.visible();
        if visible.len() < 2 {
            // 单栏时直接放窗格本身，不多包一层：
            // 外面那条 h_flex 是 items_center，多一层没有高度的 div
            // 会把列表挤成内容高度，虚拟列表就画不出几行了。
            return self.tabs[self.active].clone().into_any_element();
        }

        h_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            // 同理：位置容器在 items_center 的父级里，高度要写死成父级的满高。
            .h_full()
            .child(self.render_pane_slot(visible[0], cx))
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .flex_shrink_0()
                    .bg(cx.theme().border),
            )
            .child(self.render_pane_slot(visible[1], cx))
            .into_any_element()
    }

    /// 一栏：焦点那一栏顶上一条主色细线，让人一眼看出键盘交给谁了。
    ///
    /// 两栏都留着这条线的位置（没有焦点时用透明色），
    /// 换焦点时布局不会跟着抖一下。
    fn render_pane_slot(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let focused = index == self.active;
        let accent = if focused {
            cx.theme().primary
        } else {
            transparent_white()
        };
        v_flex()
            .id(("pane-slot", index))
            .test_support()
            .aria_label(if focused { "焦点栏" } else { "另一栏" })
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .border_t_1()
            .border_color(accent)
            .child(self.tabs[index].clone())
            .into_any_element()
    }

    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let muted = cx.theme().muted;
        let border = cx.theme().border;
        let radius = cx.theme().radius;

        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let path = tab.read(cx).path().to_path_buf();
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let is_active = index == active;

                let tab = h_flex()
                    .id(("tab", index))
                    // test_support 让集成测试能按标签序号定位；aria_label 顺带给读屏软件
                    // 报出这个标签打开的是哪个文件夹。
                    .test_support()
                    .aria_label(label.clone())
                    .h(px(26.))
                    .max_w(px(180.))
                    .pl_2()
                    .pr_1()
                    .gap_1()
                    .items_center()
                    .rounded(radius)
                    .border_1()
                    .border_color(if is_active {
                        border
                    } else {
                        transparent_white()
                    })
                    .bg(if is_active {
                        muted
                    } else {
                        transparent_white()
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(muted))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .child(Icon::new(IconName::Folder).size_3())
                            .child(div().min_w_0().truncate().text_sm().child(label)),
                    )
                    .child(
                        div()
                            .id(("tab-close", index))
                            .flex_shrink_0()
                            .size_4()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|style| style.bg(border))
                            .child(Icon::new(IconName::X).size_3())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_tab(index, window, cx)
                            })),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.focus_tab(index, window, cx)),
                    );
                // 拖到某个标签上就是放进它打开的那个目录：
                // 双栏时也能这么把东西送到另一栏去。
                Self::drop_target(tab, path, cx)
            })
            // 同上：闭包要借 `cx`，先把结果收下来再用 `cx`。
            .collect::<Vec<_>>();

        h_flex()
            .h(px(38.))
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .bg(cx.theme().sidebar)
            .children(tabs)
            .child(
                Button::new("new-tab")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip(crate::i18n::tr(cx, "新建标签页（Cmd+T）"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
            )
            .child(div().flex_1())
            .child(self.render_split_buttons(cx))
            .child(
                Button::new("open-palette")
                    .ghost()
                    .small()
                    .icon(IconName::Command)
                    .tooltip(crate::i18n::tr(cx, "命令面板（Cmd+P）"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
    }

    /// 分栏开关，以及分栏时才出现的同步浏览开关。
    fn render_split_buttons(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let split = self.split.is_some();
        let sync = self.sync;

        let mut row = h_flex().gap_1().flex_shrink_0().child(
            Button::new("toggle-split")
                .ghost()
                .small()
                .icon(IconName::LayoutDashboard)
                .tooltip(if split {
                    "取消分栏（Cmd+Alt+D）"
                } else {
                    "分栏显示（Cmd+Alt+D）"
                })
                .selected(split)
                .on_click(cx.listener(|this, _, window, cx| this.toggle_split(window, cx))),
        );

        if split {
            row = row.child(
                Button::new("toggle-sync")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip(if sync {
                        "关闭双栏同步浏览（Cmd+Alt+S）"
                    } else {
                        "双栏同步浏览（Cmd+Alt+S）"
                    })
                    .selected(sync)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_sync(window, cx))),
            );
        }
        row.into_any_element()
    }

    /// 点标签页：点的是另一栏就只挪焦点（两栏位置不动），
    /// 否则让这个标签顶替焦点那一栏。
    fn focus_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.split == Some(index) {
            self.focus_partner(cx);
            // 换的是同屏的另一栏：焦点得跟着传到它身上，否则键盘留在
            // 刚刚那一栏里（点标签却打不进字），见 `hand_keyboard_to_active_pane`。
            self.hand_keyboard_to_active_pane(window, cx);
            return;
        }
        if index < self.tabs.len() {
            self.active = index;
            self.hand_keyboard_to_active_pane(window, cx);
            self.sync_session(cx);
            cx.notify();
        }
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let error_color = cx.theme().danger;

        let content: AnyElement = if let Some(running) = self.ops.read(cx).running().cloned() {
            let ratio = running.fraction();
            let paused = running.state == ControlState::Paused;

            let mut detail = h_flex()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(div().truncate().child(running.label.clone()))
                .child(
                    div()
                        .flex_shrink_0()
                        .child(format!("{}/{}", running.done, running.total)),
                );

            // 同一份内容攒一份纯文本：读屏要它，测试也要它
            // （`label()` 读的是 aria label，读不到那一串子 div）。
            let mut text = format!("{} {}/{}", running.label, running.done, running.total);

            // 字节数、速度、剩余时间：只有真的在按字节搬东西时才报，
            // 删除和重命名没有这些量，硬凑一个 "0 B/s" 出来是噪音。
            if running.bytes_total > 0 {
                let bytes = format!(
                    "{} / {}",
                    format::file_size(running.bytes_done),
                    format::file_size(running.bytes_total)
                );
                detail = detail.child(div().flex_shrink_0().child(bytes.clone()));
                text.push_str(&format!(" {bytes}"));

                if let Some(rate) = running.bytes_per_second() {
                    let rate = format::speed(rate);
                    detail = detail.child(div().flex_shrink_0().child(rate.clone()));
                    text.push_str(&format!(" {rate}"));
                }
                // 暂停时不显示剩余时间：那会儿速度不再增长，
                // "还剩 3 分 20 秒" 会一直挂着不动，看着像坏了。
                if !paused && let Some(left) = running.remaining() {
                    let left = format!("还剩 {}", format::remaining(left));
                    detail = detail.child(div().flex_shrink_0().child(left.clone()));
                    text.push_str(&format!(" {left}"));
                }
            }
            if paused {
                detail = detail.child(div().flex_shrink_0().child(crate::i18n::tr(cx, "已暂停")));
                text.push_str(" 已暂停");
            }
            let queued = self.ops.read(cx).queued();
            if queued > 0 {
                let waiting = format!("还有 {queued} 个任务");
                detail = detail.child(div().flex_shrink_0().child(waiting.clone()));
                text.push_str(&format!(" {waiting}"));
            }
            // 限着速就写明：不然"怎么这么慢"看起来跟卡住了没两样。
            let limit = self.ops.read(cx).speed_limit();
            if limit > 0 {
                let note = format!("限速 {}", format::speed(limit));
                detail = detail.child(div().flex_shrink_0().child(note.clone()));
                text.push_str(&format!(" {note}"));
            }

            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(detail.id("status-detail").test_support().aria_label(text))
                .child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(cx.theme().muted)
                        .child(
                            div()
                                .h_full()
                                .w(relative(ratio))
                                .rounded_full()
                                .bg(cx.theme().primary),
                        ),
                )
                .into_any_element()
        } else if let Some(message) = self.ops.read(cx).last_message() {
            div()
                // 这句是 i18n 的最后一道关口：`.say(cx)` 而不是
                // `.to_string()`（后者恒给中文，英文界面下整栏不变）。
                // 挂着 id + `test_support` 是因为**没有它们就没人能验它**——
                // 「进度栏在英文界面下说英文」这条判据需要一个能读到的入口，
                // 而这是唯一一处能把那句话原样取出来的元素。
                .id("status-message")
                .test_support()
                .aria_label(message.say(cx))
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(if self.ops.read(cx).last_message_is_error() {
                    error_color
                } else {
                    muted
                })
                .child(message.say(cx))
                .into_any_element()
        } else {
            div().flex_1().into_any_element()
        };

        // 暂停/继续与中止：只在能停下来的任务上出现。
        // 删除和重命名是一口气跑完的，给按钮点了也没反应。
        let transport: AnyElement = if self
            .ops
            .read(cx)
            .running()
            .is_some_and(|running| running.can_pause)
        {
            let paused = self
                .ops
                .read(cx)
                .running()
                .is_some_and(|running| running.state == ControlState::Paused);
            h_flex()
                .gap_1()
                .flex_shrink_0()
                .child(
                    Button::new("transport")
                        .ghost()
                        .small()
                        .icon(if paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        })
                        .tooltip(if paused { "继续" } else { "暂停" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ops.update(cx, |ops, cx| {
                                if paused {
                                    ops.resume(cx)
                                } else {
                                    ops.pause(cx)
                                }
                            })
                        })),
                )
                .child(
                    Button::new("stop")
                        .ghost()
                        .small()
                        .icon(IconName::Square)
                        .tooltip(crate::i18n::tr(cx, "中止"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ops.update(cx, |ops, cx| ops.abort(cx))
                        })),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        // 重试：上一趟有没成功的项才出现。跑着的时候不摆出来——
        // 那时候位置让给暂停/中止。
        let retry: AnyElement = match self.ops.read(cx).retry_label() {
            Some(label) if self.ops.read(cx).running().is_none() => Button::new("retry")
                .ghost()
                .small()
                .icon(IconName::RotateCw)
                .tooltip(label.say(cx))
                .on_click(
                    cx.listener(|this, _, _, cx| this.ops.update(cx, |ops, cx| ops.retry(cx))),
                )
                .into_any_element(),
            _ => div().into_any_element(),
        };

        // 有几个远程文件正在本地编辑：这是个常驻提醒。不然关了连接之后，
        // 用户根本记不起来还有东西正往服务器上写。
        let editing: AnyElement = match self.remotes.read(cx).editing_count() {
            0 => div().into_any_element(),
            count => div()
                .id("status-editing")
                .test_support()
                .flex_shrink_0()
                .text_xs()
                .whitespace_nowrap()
                .text_color(muted)
                .child(crate::i18n::tr_args(
                    cx,
                    "{count} 个远程文件编辑中（保存即回传）",
                    &[("count", count.to_string())],
                ))
                .into_any_element(),
        };

        let undo_label = self.ops.read(cx).undo_label();
        let has_undo = undo_label.is_some();
        let undo_button = Button::new("undo")
            .ghost()
            .small()
            .icon(IconName::Undo2)
            // `max_w` 不是装饰：`Button` 的根节点写死 `flex_shrink_0`，
            // 在任何 flex 容器里都不会收缩。而 `describe()` 会把两个完整
            // 文件名拼进标签（"重命名 X 为 Y"），实测两个 60 字的中文名
            // 能把按钮撑到 1278 px——装在 1180 px 的窗口里，右边缘 1338 px，
            // 整个状态栏被它顶出屏幕。
            //
            // 完整描述在 tooltip 与无障碍名里都留着，标签只负责一眼看出
            // "能撤什么"，不负责读全。
            .max_w(px(260.))
            .tooltip(match &undo_label {
                Some(label) => {
                    crate::i18n::tr_args(cx, "撤销：{what}（Cmd+Z）", &[("what", label.say(cx))])
                }
                None => crate::i18n::tr(cx, "暂无可撤销的操作"),
            })
            .disabled(!has_undo)
            .when(has_undo, |button| {
                button.label(Self::undo_text(&undo_label, cx))
            })
            .on_click(
                cx.listener(|this, _, _, cx| this.ops.update(cx, |ops, cx| ops.request_undo(cx))),
            );

        // 按钮的可见标签不进无障碍名（`ElementSnapshot::label()` 对带 tooltip
        // 的按钮一律返回 None），所以外面套一层 div 把无障碍名挂上去：
        // 撤销按钮的文案是 `describe()` 拼出来的完整描述，
        // 「英文界面下它说英文」这条判据得有入口才能验。
        let undo_button: AnyElement = div()
            .id("undo-label")
            .test_support()
            .aria_label(Self::undo_text(&undo_label, cx))
            .flex_shrink_0()
            .child(undo_button)
            .into_any_element();

        h_flex()
            .id("status-bar")
            .test_support()
            .h(px(34.))
            .px_3()
            .gap_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .items_center()
            .child(content)
            .child(editing)
            .child(transport)
            .child(retry)
            .child(undo_button)
    }

    /// 撤销按钮的文案。有历史才给 `Some`。
    ///
    /// 单独一个函数是因为同一句话要在两处用：`Button::label` 拿可见文案，
    /// 外层 div 的 `aria_label` 拿无障碍名——两处得是同一句，
    /// 所以不能让它们各自调一次 `say`（万一有人只改一处就漏了）。
    fn undo_text(label: &Option<crate::i18n::Msg>, cx: &App) -> String {
        label.as_ref().map(|it| it.say(cx)).unwrap_or_default()
    }

    /// 半透明遮罩 + 居中卡片。确认、冲突、输入、批量重命名、连接五类对话框共用。
    ///
    /// 点遮罩等于取消；卡片内部的点击不会传到遮罩上。
    ///
    /// 两道约束都是量出来的，不是预防性防御：
    ///
    /// - **宽度**：卡片宽是写死的（420–620 px），而窗口可以是 520 px。
    ///   实测批量重命名那张 620 px 的卡在 520 px 窗口里 `x = -50`、
    ///   右边缘 570 px——**左右各溢出 50 px**，确认/取消两颗按钮有一半在屏幕外，
    ///   而用户在屏幕上只看到半张卡片，不知道还能往哪儿点。
    ///   做法是给遮罩加 16 px 内边距、卡片加 `max_w_full`，让它在窄窗口里
    ///   自己退到可用宽度，而不是靠裁剪。
    /// - **高度**：卡片没有高度上限，正文一多页脚就掉到窗口外面去。
    ///   做法是正文那一段自己滚，页脚留在卡片底部——**"确认/取消"永远在、
    ///   永远点得到**，这是对话框唯一不能缺的东西。
    fn overlay_card(
        title: crate::i18n::Msg,
        width: Pixels,
        cx: &mut Context<Self>,
        body: AnyElement,
        footer: AnyElement,
        on_overlay_click: impl Fn(&mut WFilesApp, &mut Window, &mut Context<WFilesApp>) + 'static,
    ) -> AnyElement {
        // 点遮罩和按 Esc 是同一件事（"算了"），所以两个回调共用一支。
        let close = Rc::new(on_overlay_click);
        let for_escape = close.clone();
        div()
            .id("overlay")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // 这 16 px 是卡片的活动余量：卡片自己再窄，也要留一圈边距，
            // 否则窄窗口下它会贴到窗口边上、边框被切掉一半。
            .px_4()
            .py_6()
            // 遮罩是卡片的**兄弟**，不是它的父级：点事件会顺着元素树往上冒，
            // 挂在父级上就等于"点卡片里任何一个按钮都会顺手把对话框关掉"——
            // 比如连接失败那句话刚写进去，对话框就没了。
            //
            // 它是 absolute 的，铺的是整个 overlay 容器，不受上面那圈内边距影响。
            .child(
                div()
                    .id("overlay-backdrop")
                    .test_support()
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            close(this, window, cx)
                        }),
                    ),
            )
            .child(
                v_flex()
                    .id("overlay-card")
                    .test_support()
                    .w(width)
                    // 窄窗口里退到可用宽度（遮罩宽度减去那圈内边距）。
                    // 用 `max_w_full` 而不是直接写死一个小宽度：正常窗口下这张卡
                    // 还是自己声明的宽度，只有装不下的时候才让位。
                    .max_w_full()
                    // 高度同理。窗口再矮，正文滚动、页脚不动。
                    .max_h_full()
                    .gap_4()
                    .p_5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    // 点击往下传是按z 序来的：遮罩虽然不是卡片的父级，
                    // 但它在卡片下面、又铺满整屏，所以卡片里任何一个按钮按下
                    // 都会"漏"到遮罩上把对话框关掉（连接失败那句话刚写进去就没了）。
                    // 这里在卡片上收住：按钮先执行，卡片把事件掐断，遮罩收不到。
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        div()
                            // 标题也是文案，`say(cx)` 不是 `to_string()`。
                            // id + `test_support` 让「对话框在英文界面下说英文」
                            // 这条判据有入口可读——五类对话框共用这张卡，
                            // 改一处就同时改了五处，得能验。
                            .id("overlay-title")
                            .test_support()
                            .aria_label(title.say(cx))
                            .font_semibold()
                            .flex_shrink_0()
                            .child(title.say(cx)),
                    )
                    // 正文是唯一会自己变长的一段，所以滚动放在它身上：
                    // 标题与页脚留在原地，页脚里的按钮永远不会被滚走。
                    .child(
                        div()
                            .id("overlay-body")
                            .test_support()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(body),
                    )
                    .child(h_flex().flex_shrink_0().justify_end().gap_2().child(footer)),
            )
            // 焦点在对话框里的输入框上时，按键事件会冒到这里——
            // 于是每个对话框都认 Esc，不用各自再挂一次。
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    for_escape(this, window, cx);
                }
            }))
            .into_any_element()
    }

    fn render_confirm(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = self.ops.read(cx).confirm().cloned() else {
            return div().into_any_element();
        };

        let body = div()
            .id("confirm-message")
            .test_support()
            .aria_label(confirm.message.say(cx))
            .text_sm()
            .child(confirm.message.say(cx))
            .into_any_element();
        let cancel = Button::new("confirm-cancel")
            .label(crate::i18n::tr(cx, "取消"))
            .ghost()
            .on_click(
                cx.listener(|this, _, _, cx| this.ops.update(cx, |ops, cx| ops.confirm_cancel(cx))),
            )
            .into_any_element();
        let mut ok = Button::new("confirm-ok").label(confirm.ok_label.say(cx));
        ok = if confirm.ok_dangerous {
            ok.danger()
        } else {
            ok.primary()
        };
        let ok = ok.on_click(
            cx.listener(|this, _, _, cx| this.ops.update(cx, |ops, cx| ops.confirm_ok(cx))),
        );

        Self::overlay_card(
            confirm.title,
            px(420.),
            cx,
            body,
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, _, cx| this.ops.update(cx, |ops, cx| ops.confirm_cancel(cx)),
        )
    }

    fn render_conflict(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(conflict) = self.ops.read(cx).conflict().cloned() else {
            return div().into_any_element();
        };

        let body = v_flex()
            .gap_2()
            .child(div().text_sm().child(format!(
                "目标文件夹里已有 {} 个同名项目，冲突的项怎么处理？",
                conflict.collisions
            )))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::i18n::tr(cx, "选择会应用到本次操作的剩余全部项目。")),
            )
            .into_any_element();

        let keep_both = Button::new("conflict-keep-both")
            .label(crate::i18n::tr(cx, "保留两者"))
            .primary()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| {
                    ops.resolve_conflict(ConflictPolicy::KeepBoth, cx)
                })
            }))
            .into_any_element();
        let replace = Button::new("conflict-replace")
            .label(crate::i18n::tr(cx, "替换"))
            .danger()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| {
                    ops.resolve_conflict(ConflictPolicy::Replace, cx)
                })
            }))
            .into_any_element();
        let skip = Button::new("conflict-skip")
            .label(crate::i18n::tr(cx, "跳过"))
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops
                    .update(cx, |ops, cx| ops.resolve_conflict(ConflictPolicy::Skip, cx))
            }))
            .into_any_element();
        let cancel =
            Button::new("conflict-cancel")
                .label(crate::i18n::tr(cx, "取消"))
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ops.update(cx, |ops, cx| ops.cancel_conflict(cx))
                }))
                .into_any_element();

        Self::overlay_card(
            crate::i18n::Message::msg("名称冲突"),
            px(460.),
            cx,
            body,
            h_flex()
                .gap_2()
                .child(cancel)
                .child(skip)
                .child(replace)
                .child(keep_both)
                .into_any_element(),
            |this, _, cx| this.ops.update(cx, |ops, cx| ops.cancel_conflict(cx)),
        )
    }

    fn render_prompt(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(prompt) = self.ops.read(cx).prompt().cloned() else {
            return div().into_any_element();
        };
        let Some(input) = self.prompt_input.clone() else {
            return div().into_any_element();
        };

        let ok_label = match prompt.action {
            PendingAction::Rename { .. } => "重命名",
            _ => "创建",
        };

        let mut body = v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(prompt.label.say(cx)),
            )
            .child(Input::new(&input).w_full());

        if let Some(error) = &prompt.error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.say(cx)),
            );
        }

        let cancel = Button::new("prompt-cancel")
            .label(crate::i18n::tr(cx, "取消"))
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_prompt(cx)))
            .into_any_element();
        let ok = Button::new("prompt-ok")
            .label(ok_label)
            .primary()
            .on_click(cx.listener(|this, _, _, cx| {
                let value = this
                    .prompt_input
                    .as_ref()
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_default();
                this.ops.update(cx, |ops, cx| ops.submit_prompt(value, cx))
            }))
            .into_any_element();

        Self::overlay_card(
            prompt.title,
            px(420.),
            cx,
            body.into_any_element(),
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, _, cx| this.dismiss_prompt(cx),
        )
    }

    /// 批量重命名：规则输入 + 实时预览列表，确认后才落盘。
    fn render_bulk(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(inputs) = self.bulk_inputs.clone() else {
            return div().into_any_element();
        };
        // 预览只列前几行：十万行 DOM 每帧重建一遍，这个对话框会直接卡死
        //（预览不是虚拟列表）。规则本身不受影响——确认后对全部选中项
        // 执行，总数与可执行数照实报。原来还每帧克隆整份 `BulkState`
        //（含全部预览、每个都带两个名字），这里只取要画的那几行。
        const PREVIEW_ROWS: usize = 200;
        let (previews, total, executable) = {
            let ops = self.ops.read(cx);
            let Some(bulk) = ops.bulk() else {
                return div().into_any_element();
            };
            (
                bulk.previews
                    .iter()
                    .take(PREVIEW_ROWS)
                    .cloned()
                    .collect::<Vec<_>>(),
                bulk.previews.len(),
                crate::bulk_rename::executable_count(&bulk.previews),
            )
        };

        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        // 每个输入框带自己的 element id：布局测试要按名字量宽度，
        // 而"名字模板"这五个框宽窄不一（260/120/120/80/70），只能一把把量。
        let labeled = |id: &'static str, label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .id(id)
                .test_support()
                .gap_1()
                .min_w_0()
                .child(div().text_xs().text_color(muted).child(label))
                .child(Input::new(input).w_full())
        };

        // 五个框分两行，跟地址栏 / 工具栏分两行是同一个道理。
        //
        // 一行摆不下：260+120+120+80+70 = 650，加四个 12px 间隙 = 698 px，
        // 而 620 px 的卡片扣掉两 padding 只剩 580 px。挤在一行的后果不是
        // "窄了点"，是**五个框全都在缩**——实测 1180 px 的宽窗口下也一样：
        // 模板 260 → 212、查找 120 → 98、步长 70 → 57，每个框都被按比例削掉
        // 十几个像素。窗口越宽越显得莫名其妙，因为问题跟窗口根本没关系。
        //
        // 模板是主角（{name}_{n}_副本 这一串要打得下），给它一整行；
        // 四个参数是配角，第二行 390 + 36 = 426 px 装得下。
        let rules = v_flex()
            .id("bulk-rules")
            .test_support()
            .gap_3()
            .child(labeled("bulk-template", "名字模板", &inputs.template).w_full())
            .child(
                h_flex()
                    .gap_3()
                    .items_end()
                    .child(labeled("bulk-find", "查找", &inputs.find).w(px(120.)))
                    .child(labeled("bulk-replace", "替换为", &inputs.replace).w(px(120.)))
                    .child(labeled("bulk-start", "起始序号", &inputs.start).w(px(80.)))
                    .child(labeled("bulk-step", "步长", &inputs.step).w(px(70.))),
            );

        // 预览列表：旧名 -> 新名，跳过/冲突的标出来。
        let rows = previews.iter().enumerate().map(|(index, item)| {
            let status: AnyElement = if let Some(error) = &item.error {
                div()
                    .text_xs()
                    .text_color(danger)
                    .child(error.say(cx))
                    .into_any_element()
            } else if item.collision {
                div()
                    .text_xs()
                    .text_color(danger)
                    .child(crate::i18n::tr(cx, "名称冲突"))
                    .into_any_element()
            } else if item.unchanged {
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr(cx, "无变化"))
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr(cx, "将执行"))
                    .into_any_element()
            };

            h_flex()
                .id(("bulk-row", index))
                .test_support()
                .w_full()
                .gap_2()
                .items_center()
                .py_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(item.old_name.clone()),
                )
                .child(Icon::new(IconName::ArrowRight).size_3().text_color(muted))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(item.new_name.clone()),
                )
                .child(div().flex_shrink_0().w(px(64.)).child(status))
        });

        // 截断时把话说明白：预览只列前几行，确认后全部照规则执行。
        let capped_note = (total > previews.len()).then(|| {
            let text = crate::i18n::tr_args(
                cx,
                "预览只列前 {shown} 项（共 {total} 项）；确认后全部按规则执行",
                &[
                    ("shown", PREVIEW_ROWS.to_string()),
                    ("total", total.to_string()),
                ],
            );
            div()
                .id("bulk-preview-note")
                .test_support()
                .aria_label(text.clone())
                .text_xs()
                .text_color(muted)
                .child(text)
        });

        let body = v_flex()
            .gap_3()
            .child(rules)
            .child(div().text_xs().text_color(muted).child(crate::i18n::tr(
                cx,
                "占位符：{name} 原主干 · {ext} 原扩展名（含点）· {n} 序号",
            )))
            .child(
                div()
                    .id("bulk-preview")
                    .test_support()
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .p_2()
                    .child(v_flex().gap_1().children(rows)),
            )
            .when_some(capped_note, |this, note| this.child(note))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr_args(
                        cx,
                        "共 {total} 项，其中 {executable} 项会被执行",
                        &[
                            ("total", total.to_string()),
                            ("executable", executable.to_string()),
                        ],
                    )),
            )
            .into_any_element();

        let cancel = Button::new("bulk-cancel")
            .label(crate::i18n::tr(cx, "取消"))
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_bulk(cx)))
            .into_any_element();
        let ok = Button::new("bulk-ok")
            .label(crate::i18n::tr(cx, "执行重命名"))
            .primary()
            .disabled(executable == 0)
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| ops.apply_bulk_rename(cx))
            }))
            .into_any_element();

        Self::overlay_card(
            crate::i18n::Message::args("批量重命名（{total} 项）", &[("total", total.to_string())]),
            px(620.),
            cx,
            body,
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, _, cx| this.dismiss_bulk(cx),
        )
    }

    /// 连接远程服务器：四个框 + 试连。
    ///
    /// 不做"先存下来再说"：地址打错一个字就是一个永远打不开的连接，
    /// 而用户还得自己去找怎么删。所以这里一定先连一次，通了才存。
    fn render_connect(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.connect.clone() else {
            return div().into_any_element();
        };

        let muted = cx.theme().muted_foreground;
        let labeled = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .min_w_0()
                .child(div().text_xs().text_color(muted).child(label))
                .child(Input::new(input).w_full())
        };

        // 先挑协议：不同协议要填的栏位不一样，选错了填也是白填。
        let kinds = RemoteKind::ALL
            .iter()
            .map(|kind| {
                let id = kind_id(*kind);
                Button::new(id.clone())
                    .label(kind.label())
                    .ghost()
                    .selected(state.kind == *kind)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(state) = this.connect.as_mut() {
                            state.kind = *kind;
                            state.error = None;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let mut body = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(crate::i18n::tr(cx, "协议")),
                    )
                    .child(h_flex().gap_2().children(kinds)),
            )
            .child(labeled("名称（可留空，默认取主机名）", &state.name))
            .child(labeled("服务器地址", &state.url))
            .child(labeled(state.kind.user_label(), &state.user))
            .child(labeled(state.kind.password_label(), &state.password));
        // 地区只有 S3 用：它是签名的一部分，填错的表现跟密钥错了一模一样，
        // 所以这一栏要能自己填、也写进配置让人事后看得见。
        if state.kind == RemoteKind::S3 {
            body = body.child(
                div()
                    .id("connect-region")
                    .test_support()
                    .child(labeled("地区（MinIO 留空即可）", &state.region)),
            );
        }
        // 私钥只有 SFTP 用：留空就用密码登录，填了就走密钥登录
        // （这时候下面那栏密码是私钥的口令）。
        if state.kind.has_key() {
            body = body.child(
                div()
                    .id("connect-key")
                    .test_support()
                    .child(labeled(state.kind.key_label(), &state.key)),
            );
        }
        // WebDAV 有现成的国内服务：坚果云的地址每次手打一遍还容易打错，
        // 点一下就填上（填完照样能改）。
        if state.kind == RemoteKind::WebDav {
            let url = state.url.clone();
            body = body.child(
                div().id("connect-presets").test_support().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(crate::i18n::tr(cx, "常用服务")),
                        )
                        .child(
                            Button::new("connect-preset-jianguoyun")
                                .label(crate::i18n::tr(cx, "坚果云"))
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |_, _, window, cx| {
                                    url.update(cx, |input, cx| {
                                        input.set_value(
                                            "https://dav.jianguoyun.com/dav/",
                                            window,
                                            cx,
                                        );
                                    });
                                })),
                        ),
                ),
            );
        }
        body = body.child(div().text_xs().text_color(muted).child(format!(
            "地址形如 {}。密码只进系统钥匙串，不写进配置文件。",
            state.kind.url_hint()
        )));

        if let Some(error) = &state.error {
            body = body.child(
                div()
                    .id("connect-error")
                    .test_support()
                    .aria_label(error.say(cx))
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.say(cx)),
            );
        }
        if state.busy {
            body = body.child(
                div()
                    .id("connect-busy")
                    .test_support()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::i18n::tr(cx, "正在连接…")),
            );
        }

        let cancel = Button::new("connect-cancel")
            .label(crate::i18n::tr(cx, "取消"))
            .ghost()
            .on_click(cx.listener(|this, _, window, cx| this.close_connect(window, cx)))
            .into_any_element();
        let ok = Button::new("connect-ok")
            .label(crate::i18n::tr(cx, "连接"))
            .primary()
            .disabled(state.busy)
            .on_click(cx.listener(|this, _, _, cx| this.submit_connect(cx)))
            .into_any_element();

        Self::overlay_card(
            crate::i18n::Message::msg("连接远程服务器"),
            px(460.),
            cx,
            body.into_any_element(),
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, window, cx| this.close_connect(window, cx),
        )
    }

    // ---- Quick Look ----

    /// 当前显示的那几栏里，有哪一栏正开着 Quick Look。
    ///
    /// 双栏时两个窗格各自能开各自的 Quick Look（"看一眼左栏这个文件"），
    /// 但只画当前显示的那一栏的——另一栏的浮层盖在上面会让用户以为在
    /// 看左栏，实际看的是右栏。
    fn quick_look_of_visible_pane(&self, cx: &App) -> Option<QuickLookState> {
        for index in self.visible() {
            let look = self.tabs[index].read(cx).quick_look_snapshot();
            if look.is_some() {
                return look;
            }
        }
        None
    }

    /// Quick Look 浮层：居中卡片，内容按类型分派。
    ///
    /// **画在整个窗口之上**，跟标签栏、侧栏、状态栏一起盖住——
    /// 它是"看一眼整个文件"，不是某一栏的事。挂在窗格里的话遮罩只盖住
    /// 侧栏右边的部分：实测 1180px 的窗口里遮罩从 x=210 开始，标签栏和
    /// 侧栏都亮着；520px 的窗口里窗格只剩 310px，那张卡片根本没法看。
    ///
    /// **没做"点背景关掉"**：GPUI 的点击会冒泡到父元素，卡片里选中一段文字
    /// 也会被当成点了背景，一选就关太难受。关闭走 Esc、再按一次空格，
    /// 或者卡片底部的按钮。
    fn render_quick_look(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(look) = self.quick_look_of_visible_pane(cx) else {
            return div().into_any_element();
        };

        // 大图在这一帧问一次就够了，标题和正文共用。
        let thumbnail = self.thumbs.update(cx, |thumbs, cx| {
            thumbs.get(&look.path, crate::thumbnails::PREVIEW_EDGE, cx)
        });
        let subtitle = Self::quick_look_subtitle(&look, thumbnail.clone());
        let body = match &look.content {
            Preview::Text(text) => Self::quick_look_text(text, cx),
            Preview::Hex(hex) => Self::quick_look_hex(hex, cx),
            Preview::Opaque(kind) if *kind == crate::preview::Kind::Image => {
                Self::quick_look_image(thumbnail.clone(), cx)
            }
            Preview::Opaque(kind) => {
                Self::quick_look_placeholder(crate::pane::icon_for_kind(*kind), &look.name, cx)
            }
            Preview::Directory => {
                Self::quick_look_placeholder(IconName::FolderOpen, &look.name, cx)
            }
            Preview::Unavailable(reason) => {
                Self::quick_look_placeholder(IconName::File, reason, cx)
            }
        };

        let muted = cx.theme().muted_foreground;
        // 关掉谁：显示着 Quick Look 的那一栏。双栏时是左或右，不能写死一个。
        let owner = self
            .visible()
            .into_iter()
            .find(|index| self.tabs[*index].read(cx).quick_look_open());
        let pane = owner.map(|index| self.tabs[index].clone());

        div()
            .id("quick-look")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // 跟对话框同一道约束：卡片宽写死 720 px，窗口可以是 520 px。
            .px_4()
            .py_6()
            .bg(cx.theme().overlay)
            .child(
                v_flex()
                    .id("quick-look-card")
                    .test_support()
                    .w(px(720.))
                    .max_w_full()
                    .max_h_full()
                    .gap_3()
                    .p_5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(
                        v_flex()
                            .flex_shrink_0()
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
                    // 正文自己滚：文件有几千行时标题与「关闭」按钮留在原地。
                    .child(div().flex_1().min_h_0().child(body))
                    .child(
                        h_flex().flex_shrink_0().justify_end().child(
                            Button::new("quick-look-close")
                                .label(crate::i18n::tr(cx, "关闭（Esc）"))
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if let Some(pane) = pane.as_ref() {
                                        pane.update(cx, |pane, cx| pane.close_quick_look_from(cx));
                                    }
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 卡片标题下面那一行：类型、规模，以及"这份不是全文"。
    fn quick_look_subtitle(
        look: &QuickLookState,
        thumbnail: Option<crate::thumbnails::Thumbnail>,
    ) -> String {
        match &look.content {
            Preview::Text(text) => {
                let language = text.language.unwrap_or("文本");
                format!(
                    "{language} · {} 行{}",
                    text.lines,
                    if text.truncated {
                        "（只显示开头）"
                    } else {
                        ""
                    }
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
    fn quick_look_text(text: &crate::preview::TextPreview, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        div()
            .id("quick-look-text")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .font_family(mono)
            .text_sm()
            .child(
                v_flex().children(
                    text.text
                        .lines()
                        .map(|line| div().whitespace_nowrap().child(line.to_string())),
                ),
            )
            .into_any_element()
    }

    /// 二进制：hex 转储，每行已经排好版。
    fn quick_look_hex(hex: &crate::preview::HexPreview, cx: &mut Context<Self>) -> AnyElement {
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
    fn quick_look_image(
        thumbnail: Option<crate::thumbnails::Thumbnail>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(thumbnail) = thumbnail else {
            return Self::quick_look_placeholder(IconName::GalleryVerticalEnd, "正在生成预览…", cx);
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
    fn quick_look_placeholder(icon: IconName, message: &str, cx: &mut Context<Self>) -> AnyElement {
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
}

impl Render for WFilesApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.capture_layout(window, cx);
        let has_overlay = self.ops.read(cx).confirm().is_some()
            || self.ops.read(cx).conflict().is_some()
            || self.ops.read(cx).prompt().is_some()
            || self.ops.read(cx).bulk().is_some()
            || self.palette.is_some()
            || self.connect.is_some();

        let mut root = v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_tab_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    // `min_w_0` 不是可选的：flex 容器的最小宽度默认是
                    // `auto`，也就是内容有多宽这一栏就要有多宽。工具栏上钉着
                    // 十几颗不可收缩的按钮（实测 710px），少了这一行，
                    // 窗口缩到多窄这一栏都不肯缩——侧栏与详情板被挤出屏幕，
                    // 而工具栏上一排按钮还在屏幕右边点不到。
                    .min_w_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_panes(cx)),
            );
        // 终端面板压在窗格下面、状态栏上面：它属于"当前窗口"而不是某个窗格，
        // 所以不去动窗格那一栏的布局。
        if let Some(terminal) = self.term.clone() {
            root = root.child(terminal.into_any_element());
        }
        root = root
            .child(self.render_status_bar(cx))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(
                cx.listener(|this, _: &CloseTab, window, cx| {
                    this.close_tab(this.active, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.next_tab(window, cx)))
            .on_action(
                cx.listener(|this, _: &TogglePalette, window, cx| this.toggle_palette(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleSplit, window, cx| this.toggle_split(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SwapPane, window, cx| this.focus_other(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSync, window, cx| this.toggle_sync(window, cx)))
            .on_action(
                cx.listener(|this, _: &CopyToOther, _, cx| this.transfer_to_other(false, cx)),
            )
            .on_action(cx.listener(|this, _: &MoveToOther, _, cx| this.transfer_to_other(true, cx)))
            .on_action(
                cx.listener(|this, _: &ToggleTerminal, window, cx| {
                    this.toggle_terminal(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleDetails, _, cx| this.toggle_details(cx)))
            .on_action(cx.listener(|this, _: &Compare, window, cx| this.toggle_compare(window, cx)))
            .on_action(cx.listener(|this, _: &Diff, window, cx| this.open_diff(window, cx)));

        // 比较面板自带遮罩，压在窗格上；确认框与冲突框还要在它之上，
        // 所以放在 has_overlay 那一段之前挂。
        if let Some(panel) = self.compare.clone() {
            root = root.child(panel.into_any_element());
        }
        if let Some(panel) = self.diff.clone() {
            root = root.child(panel.into_any_element());
        }

        if has_overlay {
            // 一次只显示一层：命令面板 > 连接 > 冲突 > 批量 > 输入 > 确认。
            let overlay = if self.palette.is_some() {
                self.render_palette(cx)
            } else if self.connect.is_some() {
                self.render_connect(cx)
            } else if self.ops.read(cx).conflict().is_some() {
                self.render_conflict(cx)
            } else if self.ops.read(cx).bulk().is_some() {
                self.render_bulk(cx)
            } else if self.ops.read(cx).prompt().is_some() {
                self.render_prompt(cx)
            } else {
                self.render_confirm(cx)
            };
            root = root.child(overlay);
        }

        // Quick Look 压在对话框之上：对话框是"我必须先处理这件事"，
        // Quick Look 只是一眼，两者同时开着时对话框优先。
        if self.quick_look_of_visible_pane(cx).is_some() {
            root = root.child(self.render_quick_look(cx));
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::host_of;

    #[test]
    fn the_default_name_comes_from_the_host() {
        assert_eq!(
            host_of("https://dav.jianguoyun.com/dav/").as_deref(),
            Some("dav.jianguoyun.com")
        );
        // 没有协议头、带端口、带查询串都得认出来。
        assert_eq!(
            host_of("host.local:8080/dav").as_deref(),
            Some("host.local")
        );
        assert_eq!(host_of("http://host/dav?a=b").as_deref(), Some("host"));
        // 认不出就交给调用方用兜底名字，别拿一个空串当名字。
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("/dav/"), None);
    }
}
