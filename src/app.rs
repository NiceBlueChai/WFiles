//! 应用根视图：标签栏、当前浏览窗格、文件操作对话框与底部状态栏。
//!
//! 标签页与会话恢复在这一层：每个标签是一个独立的 `Entity<Pane>`，
//! 打开的目录列表在变化时写盘，下次启动原样恢复。

use std::path::PathBuf;

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
use crate::ops::ConflictPolicy;
use crate::ops_entity::{Ops, OpsEvent, PendingAction};
use crate::palette::{self, Command as PaletteCommand};
use crate::pane::{Pane, PaneCommand, PaneEvent};
use crate::session::Session;
use crate::thumbnails::Thumbnails;
use crate::settings::{Layout, Settings};
use crate::terminal;

gpui_kit::actions!(wfiles, [NewTab, CloseTab, NextTab, TogglePalette]);

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

/// 面板能触发的动作。窗格内的操作转交给当前窗格，
/// 应用级的（标签页）由应用自己处理。
#[derive(Clone)]
enum PaletteAction {
    GoTo(PathBuf),
    SwitchTab(usize),
    Pane(PaneCommand),
    /// 把某个终端设为默认；只改设置，不打开窗口。
    SetTerminal(String),
    NewTab,
    CloseTab,
    NextTab,
}

/// 命令面板的当前状态。候选项在打开时快照，
/// 这样面板开着的时候改不到列表，行为可预测。
struct PaletteState {
    items: Vec<PaletteItem>,
    /// 过滤后的下标，按相关度排序。
    filtered: Vec<usize>,
    selected: usize,
}


/// 应用级快捷键：不依赖任何上下文，任何地方按下都生效。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("cmd-p", TogglePalette, None),
    ]);
}

pub struct WFilesApp {
    ops: Entity<Ops>,
    /// 缩略图缓存：所有标签共用一份，同一个文件只解一次。
    thumbs: Entity<Thumbnails>,
    tabs: Vec<Entity<Pane>>,
    active: usize,
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
    /// 退出时存盘的订阅；必须一直持有，否则回调会被丢弃。
    quit_subscription: Option<Subscription>,
    subscriptions: Vec<Subscription>,
}

impl WFilesApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 设置要在建窗格之前装好：窗格渲染时要读“默认终端”。
        Settings::install(cx);
        let ops = cx.new(|_| Ops::new());
        let thumbs = cx.new(|_| Thumbnails::new());

        // 恢复上次的标签；一个都不剩就开主页。
        let session = Session::load().sanitized();
        let mut tabs: Vec<Entity<Pane>> = Vec::new();
        for path in &session.tabs {
            let tab = cx.new(|cx| {
                Pane::new(path.clone(), ops.clone(), thumbs.clone(), window, cx)
            });
            tabs.push(tab);
        }
        if tabs.is_empty() {
            let tab = cx.new(|cx| {
                Pane::new(Self::initial_dir(), ops.clone(), thumbs.clone(), window, cx)
            });
            tabs.push(tab);
        }
        let active = session.active.min(tabs.len() - 1);

        let mut app = Self {
            ops: ops.clone(),
            thumbs,
            tabs,
            active,
            prompt_input: None,
            bulk_inputs: None,
            palette: None,
            palette_input: None,
            saved_session: Session::default(),
            layout: None,
            quit_subscription: None,
            subscriptions: Vec::new(),
        };

        app.subscriptions.push(cx.subscribe_in(
            &ops,
            window,
            |this, _, event: &OpsEvent, window, cx| this.on_ops_event(event, window, cx),
        ));
        for tab in &app.tabs {
            app.subscriptions.push(cx.subscribe_in(
                tab,
                window,
                |this, _, _: &PaneEvent, _, cx| this.sync_session(cx),
            ));
        }
        app.saved_session = app.snapshot(cx);

        // 退出前再存一次，覆盖“最后一次导航还没来得及落盘”的窗口。
        app.quit_subscription = Some(cx.on_app_quit(|this, cx| {
            this.sync_session(cx);
            this.save_layout(cx);
            std::future::ready(())
        }));

        app
    }

    /// 记下窗口现在的位置与尺寸。
    ///
    /// 在渲染时取而不是等到退出时：退出回调拿不到 `Window`，
    /// 而渲染每一帧都有窗口句柄，顺便读一下是零成本的。
    fn capture_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let layout = self.layout;
        Settings::update(cx, |settings| settings.layout = layout);
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

    fn navigate_active_tab(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs[self.active].update(cx, |pane, cx| {
            pane.navigate_to(path, true, window, cx)
        });
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.tabs[self.active].read(cx).path().to_path_buf();
        let items = Self::places()
            .into_iter()
            .enumerate()
            .map(|(index, (label, icon, path))| {
                let target = path.clone();
                ListItem::new(("place", index))
                    .rounded(cx.theme().radius)
                    .selected(current == path)
                    .accessibility_label(label)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Icon::new(icon).size_4())
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate_active_tab(target.clone(), window, cx);
                    }))
            });

        v_flex()
            .w(px(210.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .h(px(52.))
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
                        v_flex()
                            .child(div().font_semibold().child("WFiles"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("快速、清楚、可撤销"),
                            ),
                    ),
            )
            .child(
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
                            .child("位置"),
                    )
                    .children(items),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .m_2()
                    .p_3()
                    .gap_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(Icon::new(IconName::HardDrive).size_4())
                    .child("本地文件系统"),
            )
    }

    // ---- 标签管理 ----

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = cx.new(|cx| {
            Pane::new(
                Self::initial_dir(),
                self.ops.clone(),
                self.thumbs.clone(),
                window,
                cx,
            )
        });
        self.subscriptions.push(cx.subscribe_in(
            &tab,
            window,
            |this, _, _: &PaneEvent, _, cx| this.sync_session(cx),
        ));
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.sync_session(cx);
        cx.notify();
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
            self.sync_session(cx);
            cx.notify();
            return;
        }

        self.tabs.remove(index);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.sync_session(cx);
        cx.notify();
    }

    fn next_tab(&mut self, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.sync_session(cx);
        cx.notify();
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
        }
    }

    /// 有变化才写盘；写失败不影响使用（下次启动退回默认会话）。
    fn sync_session(&mut self, cx: &mut Context<Self>) {
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

    // ---- 文件操作事件 ----

    fn on_ops_event(&mut self, event: &OpsEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            OpsEvent::Changed {
                removed,
                changed,
                renamed_to,
            } => {
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
                        let input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .default_value(initial)
                                .placeholder("名称")
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
                                    this.ops
                                        .update(cx, |ops, cx| ops.submit_prompt(value, cx));
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
                }
            }
            OpsEvent::BulkChanged { open } => {
                if *open {
                    if self.bulk_inputs.is_none() {
                        self.create_bulk_inputs(window, cx);
                    }
                } else {
                    self.bulk_inputs = None;
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
            inputs.template.update(cx, |input, cx| input.focus(window, cx));
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
            self.close_palette(cx);
        } else {
            self.open_palette(window, cx);
        }
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
        let commands: Vec<PaletteCommand> =
            items.iter().map(|item| item.command.clone()).collect();
        self.palette = Some(PaletteState {
            items,
            filtered: palette::filter(&commands, ""),
            selected: 0,
        });

        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("搜索命令、位置或标签页")
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

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_input = None;
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
        let commands: Vec<PaletteCommand> =
            state.items.iter().map(|item| item.command.clone()).collect();
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

    fn run_palette_action(
        &mut self,
        action: PaletteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_palette(cx);
        match action {
            PaletteAction::GoTo(path) => self.navigate_active_tab(path, window, cx),
            PaletteAction::SwitchTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                    self.sync_session(cx);
                    cx.notify();
                }
            }
            PaletteAction::Pane(command) => {
                self.tabs[self.active]
                    .update(cx, |pane, cx| pane.run_command(command, window, cx));
            }
            PaletteAction::SetTerminal(id) => {
                Settings::update(cx, |settings| settings.terminal = id.clone());
            }
            PaletteAction::NewTab => self.new_tab(window, cx),
            PaletteAction::CloseTab => self.close_tab(self.active, window, cx),
            PaletteAction::NextTab => self.next_tab(cx),
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
        ] {
            if !self.pane_command_enabled(command, cx) {
                continue;
            }
            let (title, hint, keywords) = Self::describe_pane_command(command);
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("pane:{command:?}"),
                    title,
                    hint,
                    "文件",
                    keywords,
                ),
                action: PaletteAction::Pane(command),
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
                    format!("前往{label}"),
                    String::new(),
                    "前往",
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
            let hint = if index == self.active { "当前" } else { "" };
            items.push(PaletteItem {
                command: PaletteCommand::new(
                    format!("tab:{index}"),
                    format!("切换到标签页：{label}"),
                    hint,
                    "标签页",
                    "tab 切换 窗口",
                ),
                action: PaletteAction::SwitchTab(index),
            });
        }

        items.push(PaletteItem {
            command: PaletteCommand::new("app:new-tab", "新建标签页", "Cmd+T", "标签页", "tab 新开"),
            action: PaletteAction::NewTab,
        });
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:close-tab",
                "关闭标签页",
                "Cmd+W",
                "标签页",
                "tab 关闭",
            ),
            action: PaletteAction::CloseTab,
        });
        items.push(PaletteItem {
            command: PaletteCommand::new(
                "app:next-tab",
                "下一个标签页",
                "Ctrl+Tab",
                "标签页",
                "tab 切换",
            ),
            action: PaletteAction::NextTab,
        });

        // 换默认终端：只列装了的，免得选一个按下去报错的。
        let current = Settings::read(cx).terminal;
        items.push(PaletteItem {
            command: PaletteCommand::new(
                format!("terminal:{}", terminal::AUTO),
                "默认终端：跟随系统",
                if current == terminal::AUTO { "当前" } else { "" },
                "终端",
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
                    format!("默认终端：{}", option.name),
                    if current == option.id { "当前" } else { "" },
                    "终端",
                    "terminal shell 命令行",
                ),
                action: PaletteAction::SetTerminal(option.id.to_string()),
            });
        }

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
            _ => true,
        }
    }

    fn describe_pane_command(command: PaneCommand) -> (&'static str, &'static str, &'static str) {
        match command {
            PaneCommand::NewFolder => ("新建文件夹", "Cmd+Shift+N", "folder 创建 目录"),
            PaneCommand::Rename => ("重命名", "F2", "rename 改名"),
            PaneCommand::BulkRename => ("批量重命名", "Cmd+Shift+R", "batch 序号 模板"),
            PaneCommand::Trash => ("移到回收站", "Cmd+Backspace", "delete 删除 trash"),
            PaneCommand::Copy => ("拷贝选中项", "Cmd+C", "copy"),
            PaneCommand::Cut => ("剪切选中项", "Cmd+X", "cut 移动"),
            PaneCommand::Paste => ("粘贴到当前目录", "Cmd+V", "paste"),
            PaneCommand::Undo => ("撤销上一步操作", "Cmd+Z", "undo 恢复"),
            PaneCommand::Refresh => ("刷新当前目录", "Cmd+R", "refresh 重新读取"),
            PaneCommand::GoUp => ("前往上一级", "Backspace", "up parent 上级"),
            PaneCommand::CopyPath => ("复制路径", "Cmd+Alt+C", "path 剪贴板"),
            PaneCommand::CopyFileName => ("复制文件名", "Cmd+Shift+C", "name 剪贴板"),
            PaneCommand::OpenTerminal => {
                ("在这里打开终端", "Cmd+Alt+T", "terminal 终端 shell 命令行")
            }
            PaneCommand::QuickLook => {
                ("快速查看", "空格", "preview 预览 quick look 查看")
            }
        }
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
                    .bg(if is_selected { selected_bg } else { transparent_white() })
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
                .child("没有匹配的命令")
                .into_any_element()
        } else {
            // overflow_y_scroll 属于 StatefulInteractiveElement，
            // 只有带 id 的 Stateful<Div> 才有，所以先落一个 id。
            let list = div()
                .id("palette-list")
                .flex()
                .flex_col()
                .max_h(px(380.))
                .overflow_y_scroll()
                .p_1()
                .children(rows);
            list.into_any_element()
        };

        let card = v_flex()
            .w(px(600.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(46.))
                    .border_b_1()
                    .border_color(border)
                    .child(Icon::new(IconName::Search).size_4().text_color(muted))
                    .child(Input::new(&input).cleanable(true).w_full()),
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
                    .child("Enter 执行")
                    .child("Esc 关闭"),
            );

        div()
            .id("palette-overlay")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(90.))
            .bg(cx.theme().overlay)
            .on_click(cx.listener(|this, _, _, cx| this.close_palette(cx)))
            .child(card)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "up" => this.move_palette_selection(-1, cx),
                    "down" => this.move_palette_selection(1, cx),
                    "enter" => this.confirm_palette(window, cx),
                    "escape" => this.close_palette(cx),
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

    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let muted = cx.theme().muted;
        let border = cx.theme().border;
        let radius = cx.theme().radius;

        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            let path = tab.read(cx).path().to_path_buf();
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let is_active = index == active;

            h_flex()
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
                .border_color(if is_active { border } else { transparent_white() })
                .bg(if is_active { muted } else { transparent_white() })
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = index;
                    this.sync_session(cx);
                    cx.notify();
                }))
        });

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
                    .tooltip("新建标签页（Cmd+T）")
                    .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("open-palette")
                    .ghost()
                    .small()
                    .icon(IconName::Command)
                    .tooltip("命令面板（Cmd+P）")
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let error_color = cx.theme().danger;

        let content: AnyElement = if let Some(running) = self.ops.read(cx).running() {
            let ratio = if running.total == 0 {
                0.
            } else {
                (running.done as f32 / running.total as f32).clamp(0., 1.)
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(muted)
                        .child(div().truncate().child(running.label.clone()))
                        .child(
                            div()
                                .flex_shrink_0()
                                .child(format!("{}/{}", running.done, running.total)),
                        ),
                )
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
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(if self.ops.read(cx).last_message_is_error() {
                    error_color
                } else {
                    muted
                })
                .child(message.to_string())
                .into_any_element()
        } else {
            div().flex_1().into_any_element()
        };

        let undo_label = self.ops.read(cx).undo_label();
        let has_undo = undo_label.is_some();
        let undo_button = Button::new("undo")
            .ghost()
            .small()
            .icon(IconName::Undo2)
            .tooltip(match &undo_label {
                Some(label) => format!("撤销：{label}（Cmd+Z）"),
                None => "暂无可撤销的操作".to_string(),
            })
            .disabled(!has_undo)
            .when(has_undo, |button| {
                button.label(undo_label.clone().unwrap_or_default())
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| ops.request_undo(cx))
            }));

        h_flex()
            .h(px(34.))
            .px_3()
            .gap_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .items_center()
            .child(content)
            .child(undo_button)
    }

    /// 半透明遮罩 + 居中卡片。确认、冲突、输入三类对话框共用。
    ///
    /// 点遮罩等于取消；卡片内部的点击不会传到遮罩上。
    fn overlay_card(
        title: String,
        width: Pixels,
        cx: &mut Context<Self>,
        body: AnyElement,
        footer: AnyElement,
        on_overlay_click: impl Fn(&mut WFilesApp, &mut Window, &mut Context<WFilesApp>) + 'static,
    ) -> AnyElement {
        div()
            .id("overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                on_overlay_click(this, window, cx)
            }))
            .child(
                v_flex()
                    .w(width)
                    .gap_4()
                    .p_5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(div().font_semibold().child(title))
                    .child(body)
                    .child(h_flex().justify_end().gap_2().child(footer)),
            )
            .into_any_element()
    }

    fn render_confirm(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = self.ops.read(cx).confirm().cloned() else {
            return div().into_any_element();
        };

        let body = div().text_sm().child(confirm.message.clone()).into_any_element();
        let cancel = Button::new("confirm-cancel")
            .label("取消")
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| ops.confirm_cancel(cx))
            }))
            .into_any_element();
        let mut ok = Button::new("confirm-ok").label(confirm.ok_label.clone());
        ok = if confirm.ok_dangerous {
            ok.danger()
        } else {
            ok.primary()
        };
        let ok = ok.on_click(cx.listener(|this, _, _, cx| {
            this.ops.update(cx, |ops, cx| ops.confirm_ok(cx))
        }));

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
            .child(
                div()
                    .text_sm()
                    .child(format!(
                        "目标文件夹里已有 {} 个同名项目，冲突的项怎么处理？",
                        conflict.collisions
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("选择会应用到本次操作的剩余全部项目。"),
            )
            .into_any_element();

        let keep_both = Button::new("conflict-keep-both")
            .label("保留两者")
            .primary()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops
                    .update(cx, |ops, cx| ops.resolve_conflict(ConflictPolicy::KeepBoth, cx))
            }))
            .into_any_element();
        let replace = Button::new("conflict-replace")
            .label("替换")
            .danger()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops
                    .update(cx, |ops, cx| ops.resolve_conflict(ConflictPolicy::Replace, cx))
            }))
            .into_any_element();
        let skip = Button::new("conflict-skip")
            .label("跳过")
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops
                    .update(cx, |ops, cx| ops.resolve_conflict(ConflictPolicy::Skip, cx))
            }))
            .into_any_element();
        let cancel = Button::new("conflict-cancel")
            .label("取消")
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.update(cx, |ops, cx| ops.cancel_conflict(cx))
            }))
            .into_any_element();

        Self::overlay_card(
            "名称冲突".to_string(),
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
                    .child(prompt.label.clone()),
            )
            .child(Input::new(&input).w_full());

        if let Some(error) = &prompt.error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }

        let cancel = Button::new("prompt-cancel")
            .label("取消")
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
                this.ops
                    .update(cx, |ops, cx| ops.submit_prompt(value, cx))
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
        let Some(bulk) = self.ops.read(cx).bulk().cloned() else {
            return div().into_any_element();
        };
        let Some(inputs) = self.bulk_inputs.clone() else {
            return div().into_any_element();
        };

        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        let labeled = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(label),
                )
                .child(Input::new(input).w_full())
        };

        let rules = h_flex()
            .gap_3()
            .items_end()
            .child(labeled("名字模板", &inputs.template).w(px(260.)))
            .child(labeled("查找", &inputs.find).w(px(120.)))
            .child(labeled("替换为", &inputs.replace).w(px(120.)))
            .child(labeled("起始序号", &inputs.start).w(px(80.)))
            .child(labeled("步长", &inputs.step).w(px(70.)));

        // 预览列表：旧名 -> 新名，跳过/冲突的标出来。
        let rows = bulk.previews.iter().enumerate().map(|(index, item)| {
            let status: AnyElement = if let Some(error) = &item.error {
                div().text_xs().text_color(danger).child(error.clone()).into_any_element()
            } else if item.collision {
                div().text_xs().text_color(danger).child("名称冲突").into_any_element()
            } else if item.unchanged {
                div().text_xs().text_color(muted).child("无变化").into_any_element()
            } else {
                div().text_xs().text_color(muted).child("将执行").into_any_element()
            };

            h_flex()
                .id(("bulk-row", index))
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

        let executable = crate::bulk_rename::executable_count(&bulk.previews);
        let total = bulk.previews.len();

        let body = v_flex()
            .gap_3()
            .child(rules)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("占位符：{name} 原主干 · {ext} 原扩展名（含点）· {n} 序号"),
            )
            .child(
                div()
                    .id("bulk-preview")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .p_2()
                    .child(
                        v_flex().gap_1().children(rows),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("共 {total} 项，其中 {executable} 项会被执行")),
            )
            .into_any_element();

        let cancel = Button::new("bulk-cancel")
            .label("取消")
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_bulk(cx)))
            .into_any_element();
        let ok = Button::new("bulk-ok")
            .label("执行重命名")
            .primary()
            .disabled(executable == 0)
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops
                    .update(cx, |ops, cx| ops.apply_bulk_rename(cx))
            }))
            .into_any_element();

        Self::overlay_card(
            format!("批量重命名（{total} 项）"),
            px(620.),
            cx,
            body,
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, _, cx| this.dismiss_bulk(cx),
        )
    }
}

impl Render for WFilesApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.capture_layout(window, cx);
        let has_overlay = self.ops.read(cx).confirm().is_some()
            || self.ops.read(cx).conflict().is_some()
            || self.ops.read(cx).prompt().is_some()
            || self.ops.read(cx).bulk().is_some()
            || self.palette.is_some();

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
                    .child(self.render_sidebar(cx))
                    .child(self.tabs[self.active].clone()),
            )
            .child(self.render_status_bar(cx))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                this.new_tab(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                this.close_tab(this.active, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.next_tab(cx)))
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.toggle_palette(window, cx)
            }));

        if has_overlay {
            // 一次只显示一层：命令面板 > 冲突 > 批量 > 输入 > 确认。
            let overlay = if self.palette.is_some() {
                self.render_palette(cx)
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

        root
    }
}
