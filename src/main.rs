use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use gpui_kit::assets::{Assets, IconName};
use gpui_kit::component::{
    ActiveTheme, Icon,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    scroll::ScrollableElement as _,
    *,
};
use gpui_kit::*;

#[derive(Clone)]
struct FileEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

impl FileEntry {
    fn from_path(path: PathBuf) -> Option<Self> {
        let metadata = fs::metadata(&path).ok()?;
        let name = path.file_name()?.to_string_lossy().into_owned();

        Some(Self {
            path,
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn kind_label(&self) -> String {
        if self.is_dir {
            return "文件夹".to_string();
        }

        self.path
            .extension()
            .map(|extension| format!("{} 文件", extension.to_string_lossy().to_uppercase()))
            .unwrap_or_else(|| "文件".to_string())
    }
}

struct WFilesApp {
    current_dir: PathBuf,
    entries: Vec<FileEntry>,
    selected: Option<PathBuf>,
    back_stack: Vec<PathBuf>,
    forward_stack: Vec<PathBuf>,
    search_input: Entity<InputState>,
    status: String,
    _subscriptions: Vec<Subscription>,
}

impl WFilesApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current_dir = dirs::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let entries = Self::read_entries(&current_dir).unwrap_or_default();
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("筛选当前文件夹"));
        let _subscriptions =
            vec![
                cx.subscribe_in(&search_input, window, |_, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }),
            ];

        Self {
            current_dir,
            entries,
            selected: None,
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            search_input,
            status: "就绪".to_string(),
            _subscriptions,
        }
    }

    fn read_entries(path: &Path) -> Result<Vec<FileEntry>, String> {
        let read_dir = fs::read_dir(path).map_err(|error| error.to_string())?;
        let mut entries: Vec<_> = read_dir
            .filter_map(Result::ok)
            .filter_map(|entry| FileEntry::from_path(entry.path()))
            .collect();

        entries.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(entries)
    }

    fn navigate_to(&mut self, path: PathBuf, remember: bool, cx: &mut Context<Self>) {
        match Self::read_entries(&path) {
            Ok(entries) => {
                if remember && path != self.current_dir {
                    self.back_stack.push(self.current_dir.clone());
                    self.forward_stack.clear();
                }
                self.current_dir = path;
                self.entries = entries;
                self.selected = None;
                self.status = format!("{} 个项目", self.entries.len());
            }
            Err(error) => {
                self.status = format!("无法打开：{error}");
            }
        }
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.back_stack.pop() {
            self.forward_stack.push(self.current_dir.clone());
            self.navigate_to(path, false, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.forward_stack.pop() {
            self.back_stack.push(self.current_dir.clone());
            self.navigate_to(path, false, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.navigate_to(self.current_dir.clone(), false, cx);
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_dir = self.current_dir.clone();
        let items = Self::places()
            .into_iter()
            .enumerate()
            .map(|(index, (label, icon, path))| {
                let selected = current_dir == path;
                ListItem::new(("place", index))
                    .rounded(cx.theme().radius)
                    .selected(selected)
                    .accessibility_label(label)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Icon::new(icon).size_4())
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate_to(path.clone(), true, cx);
                    }))
            });

        v_flex()
            .w(px(220.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .h(px(64.))
                    .px_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .size_8()
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
                    .p_3()
                    .gap_1()
                    .child(
                        div()
                            .px_3()
                            .pb_2()
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
                    .m_3()
                    .p_3()
                    .gap_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(Icon::new(IconName::HardDrive).size_4())
                    .child("本地文件系统"),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(64.))
            .px_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("back")
                    .icon(IconName::ChevronLeft)
                    .ghost()
                    .disabled(self.back_stack.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ChevronRight)
                    .ghost()
                    .disabled(self.forward_stack.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
            )
            .child(
                Button::new("up")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .disabled(self.current_dir.parent().is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
            )
            .child(
                div()
                    .h(px(36.))
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
                    .truncate()
                    .child(self.current_dir.display().to_string()),
            )
            .child(
                Button::new("refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                Input::new(&self.search_input)
                    .prefix(Icon::new(IconName::Search).size_4())
                    .cleanable(true)
                    .w(px(240.)),
            )
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search_input.read(cx).value().to_lowercase();
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| query.is_empty() || entry.name.to_lowercase().contains(query.as_str()))
            .cloned()
            .collect();
        let selected = self.selected.clone();
        let rows = entries.into_iter().enumerate().map(|(index, entry)| {
            let path = entry.path.clone();
            let open_path = entry.path.clone();
            let is_dir = entry.is_dir;
            let icon = if is_dir {
                IconName::Folder
            } else {
                IconName::File
            };

            ListItem::new(("entry", index))
                .h(px(40.))
                .px_3()
                .py_0()
                .selected(selected.as_ref() == Some(&entry.path))
                .accessibility_label(entry.name.clone())
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            div()
                                .w(px(24.))
                                .text_color(if is_dir {
                                    cx.theme().primary
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(Icon::new(icon).size_4()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .child(entry.name.clone()),
                        )
                        .child(
                            div()
                                .w(px(100.))
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(if is_dir {
                                    "—".to_string()
                                } else {
                                    format_size(entry.size)
                                }),
                        )
                        .child(
                            div()
                                .w(px(120.))
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(format_modified(entry.modified)),
                        )
                        .child(
                            div()
                                .w(px(90.))
                                .text_sm()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .child(entry.kind_label()),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.selected = Some(path.clone());
                        if event.click_count >= 2 {
                            if is_dir {
                                this.navigate_to(open_path.clone(), true, cx);
                            } else if let Err(error) = open::that(&open_path) {
                                this.status = format!("无法打开：{error}");
                            }
                        }
                        cx.notify();
                    }),
                )
        });

        v_flex()
            .h_full()
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().w(px(24.)))
                    .child(div().ml_3().flex_1().child("名称"))
                    .child(div().w(px(100.)).child("大小"))
                    .child(div().w(px(120.)).child("修改时间"))
                    .child(div().w(px(90.)).child("类型")),
            )
            .child(
                div()
                    .id("file-list")
                    .flex_1()
                    .overflow_y_scrollbar()
                    .p_1()
                    .children(rows),
            )
    }

    fn render_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .selected
            .as_ref()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path));

        let content = if let Some(entry) = selected {
            v_flex()
                .gap_4()
                .child(
                    div()
                        .h(px(140.))
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
                        format_size(entry.size)
                    },
                    cx,
                ))
                .child(detail_row("修改", format_modified(entry.modified), cx))
                .child(detail_row("位置", entry.path.display().to_string(), cx))
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::PanelRight).size_8())
                .child(div().text_sm().child("选择一个项目查看详情"))
                .into_any_element()
        };

        v_flex()
            .w(px(280.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .p_4()
            .child(div().pb_4().font_semibold().child("详细信息"))
            .child(content)
    }
}

impl Render for WFilesApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(
                v_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_toolbar(cx))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_file_list(cx))
                            .child(self.render_details(cx)),
                    )
                    .child(
                        h_flex()
                            .h(px(28.))
                            .px_3()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.status.clone()),
                    ),
            )
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

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_modified(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return "未知".to_string();
    };
    let Ok(elapsed) = modified.elapsed() else {
        return "刚刚".to_string();
    };

    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => "刚刚".to_string(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        _ => format!("{} 天前", seconds / 86_400),
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, |window, cx| {
            window.set_window_title("WFiles");
            cx.new(|cx| WFilesApp::new(window, cx))
        })
        .expect("Failed to open WFiles window");
    });
}
