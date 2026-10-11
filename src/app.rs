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

use crate::ops::ConflictPolicy;
use crate::ops_entity::{Ops, OpsEvent, PendingAction};
use crate::pane::{Pane, PaneEvent};
use crate::session::Session;

gpui_kit::actions!(wfiles, [NewTab, CloseTab, NextTab]);

/// 应用级快捷键：不依赖任何上下文，任何地方按下都生效。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
    ]);
}

pub struct WFilesApp {
    ops: Entity<Ops>,
    tabs: Vec<Entity<Pane>>,
    active: usize,
    /// 输入对话框（新建文件夹/重命名）的输入框，随开随建。
    prompt_input: Option<Entity<InputState>>,
    /// 最近一次落盘的会话，用来判断要不要再写一次。
    saved_session: Session,
    /// 退出时存盘的订阅；必须一直持有，否则回调会被丢弃。
    quit_subscription: Option<Subscription>,
    subscriptions: Vec<Subscription>,
}

impl WFilesApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ops = cx.new(|_| Ops::new());

        // 恢复上次的标签；一个都不剩就开主页。
        let session = Session::load().sanitized();
        let mut tabs: Vec<Entity<Pane>> = Vec::new();
        for path in &session.tabs {
            let tab = cx.new(|cx| Pane::new(path.clone(), ops.clone(), window, cx));
            tabs.push(tab);
        }
        if tabs.is_empty() {
            let tab = cx.new(|cx| Pane::new(Self::initial_dir(), ops.clone(), window, cx));
            tabs.push(tab);
        }
        let active = session.active.min(tabs.len() - 1);

        let mut app = Self {
            ops: ops.clone(),
            tabs,
            active,
            prompt_input: None,
            saved_session: Session::default(),
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
            std::future::ready(())
        }));

        app
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
        let tab = cx.new(|cx| Pane::new(Self::initial_dir(), self.ops.clone(), window, cx));
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
                            |this, _, event: &InputEvent, _, cx| match event {
                                InputEvent::PressEnter { .. } => {
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
                                _ => {}
                            },
                        ));
                        self.prompt_input = Some(input);
                    }
                } else {
                    self.prompt_input = None;
                }
            }
        }
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
                    .w(px(420.))
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
            cx,
            body.into_any_element(),
            h_flex().gap_2().child(cancel).child(ok).into_any_element(),
            |this, _, cx| this.dismiss_prompt(cx),
        )
    }
}

impl Render for WFilesApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_overlay = self.ops.read(cx).confirm().is_some()
            || self.ops.read(cx).conflict().is_some()
            || self.ops.read(cx).prompt().is_some();

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
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.next_tab(cx)));

        if has_overlay {
            // 一次只显示一层：冲突 > 输入 > 确认。
            let overlay = if self.ops.read(cx).conflict().is_some() {
                self.render_conflict(cx)
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
