//! 应用根视图：侧栏位置列表 + 当前浏览窗格。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, list::ListItem, *};
use gpui_kit::*;

use crate::pane::Pane;

pub struct WFilesApp {
    pane: Entity<Pane>,
}

impl WFilesApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(Self::initial_dir(), window, cx));
        Self { pane }
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

    fn navigate_pane(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.pane.update(cx, |pane, cx| {
            pane.navigate_to(path, true, window, cx);
        });
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.pane.read(cx).path().to_path_buf();
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
                        this.navigate_pane(target.clone(), window, cx);
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
                        v_flex().child(div().font_semibold().child("WFiles")).child(
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
}

impl Render for WFilesApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(self.pane.clone())
    }
}
