//! 文件内容比较面板：两个文件并排看，不同的地方标出来。
//!
//! 算的活在 [`crate::diff`]，这里只管三件事：
//! 把两个文件读进来（后台线程，不卡界面）、把结果按当前看法摆成列表、
//! 把 Esc / ↑↓ / 页签接上。
//!
//! 三种看法共用一份数据：**按行**（哪一行改了）、**细到字符**（一行里改了哪一段）、
//! **十六进制**（哪个字节不一样，二进制文件只有这一档可看）。

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
    Task, Window, div, px, size, transparent_white, *,
};

use crate::diff::{self, HexRow, Kind, Loaded, TextRow};

const CARD_WIDTH: f32 = 980.;
const CARD_MAX_HEIGHT: f32 = 640.;
/// 一行的高度：并排看时行要密一点，才看得出上下文。
const ROW_HEIGHT: f32 = 26.;

/// 面板用哪种看法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMode {
    /// 按行：哪一行改了、加了、少了。
    Lines,
    /// 细到字符：一行里改了哪一段。
    Chars,
    /// 十六进制：哪个字节不一样。二进制文件只有这一档。
    Hex,
}

impl DiffMode {
    pub const ALL: [Self; 3] = [Self::Lines, Self::Chars, Self::Hex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lines => "按行",
            Self::Chars => "细到字符",
            Self::Hex => "十六进制",
        }
    }
}

/// 面板告诉应用的两件事。
#[derive(Clone, Debug)]
pub enum DiffEvent {
    Close,
    /// 跳到左边那个文件（Enter）。
    Reveal(PathBuf),
}

/// 两个文件读完之后送回来的东西。
#[derive(Debug)]
pub enum LoadEvent {
    Loaded {
        left: Result<Loaded, String>,
        right: Result<Loaded, String>,
    },
}

pub struct DiffPanel {
    left_path: PathBuf,
    right_path: PathBuf,
    mode: DiffMode,
    /// 两边读进来的内容；还没读完时是 `None`。
    left: Option<Loaded>,
    right: Option<Loaded>,
    /// 读不出来（或压根不是文本）的原因。
    error: Option<String>,
    text_rows: Vec<TextRow>,
    text_stats: diff::TextStats,
    hex_rows: Vec<HexRow>,
    hex_stats: diff::HexStats,
    cursor: usize,
    task: Option<Task<()>>,
    scroll: VirtualListScrollHandle,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    focus: FocusHandle,
}

impl EventEmitter<DiffEvent> for DiffPanel {}

impl Focusable for DiffPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DiffPanel {
    pub fn new(left_path: PathBuf, right_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            left_path: left_path.clone(),
            right_path: right_path.clone(),
            // 默认按行；等真读进来发现不是文本，再切到十六进制。
            mode: DiffMode::Lines,
            left: None,
            right: None,
            error: None,
            text_rows: Vec::new(),
            text_stats: diff::TextStats::default(),
            hex_rows: Vec::new(),
            hex_stats: diff::HexStats::default(),
            cursor: 0,
            task: None,
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
            focus: cx.focus_handle(),
        };
        this.start(left_path, right_path, cx);
        this
    }

    /// 后台读两个文件。
    ///
    /// 单个文件最多 8 MB，但那也可能是网络盘上的 8 MB——
    /// 在界面线程上读会卡住整个窗口，所以一律丢到线程里去。
    fn start(&mut self, left: PathBuf, right: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::unbounded::<LoadEvent>();
        let sink: Arc<dyn Fn(LoadEvent) + Send + Sync> = Arc::new(move |event| {
            tx.try_send(event).ok();
        });
        let for_thread = sink.clone();
        let (left_t, right_t) = (left.clone(), right.clone());
        let worker = std::thread::Builder::new()
            .name("wfiles-diff".to_string())
            .spawn(move || {
                for_thread(LoadEvent::Loaded {
                    left: diff::load(&left_t),
                    right: diff::load(&right_t),
                });
            });
        if worker.is_err() {
            // 起不了线程就当场读：慢一点，但界面不会一直空着。
            sink(LoadEvent::Loaded {
                left: diff::load(&left),
                right: diff::load(&right),
            });
        }

        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        this.apply(event, cx);
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    /// 两边都读到了：算出三种看法要用的数据。
    fn apply(&mut self, event: LoadEvent, cx: &mut Context<Self>) {
        let LoadEvent::Loaded { left, right } = event;
        match (left, right) {
            (Ok(left), Ok(right)) => {
                // 不是文本（二进制）就只能按字节看：自动切到十六进制，
                // 免得用户打开看到一片乱码还以为坏了。
                if left.is_text() && right.is_text() {
                    self.mode = DiffMode::Lines;
                } else {
                    self.mode = DiffMode::Hex;
                }
                let (rows, stats) = diff::diff_hex(&left.bytes, &right.bytes);
                self.hex_rows = rows;
                self.hex_stats = stats;
                if left.truncated || right.truncated {
                    self.error = Some(format!(
                        "文件太大，只比了前 {}",
                        crate::format::file_size(diff::MAX_BYTES as u64)
                    ));
                }
                self.left = Some(left);
                self.right = Some(right);
                self.compute_text(false);
            }
            (Err(error), _) | (_, Err(error)) => {
                self.error = Some(error);
            }
        }
        self.sync_rows();
        cx.notify();
    }

    /// 当前这两个文件能用哪几种看法。
    ///
    /// 二进制文件没有"行"这回事，只给十六进制，
    /// 免得切过去看到一片空白还要猜是不是坏了。
    fn available_modes(&self) -> Vec<DiffMode> {
        let is_text = self.left.as_ref().is_some_and(|side| side.is_text())
            && self.right.as_ref().is_some_and(|side| side.is_text());
        if is_text {
            DiffMode::ALL.to_vec()
        } else {
            vec![DiffMode::Hex]
        }
    }

    fn set_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.cursor = 0;
        // "细到字符"要另算一遍：行内哪一段改了是按字符比对出来的，
        // 按行那一档没算这个，直接切过去会没高亮。
        if mode != DiffMode::Hex {
            self.compute_text(mode == DiffMode::Chars);
        }
        self.sync_rows();
        cx.notify();
    }

    /// 按文本的两种看法重算一遍行。
    fn compute_text(&mut self, char_level: bool) {
        let Some(left) = self.left.as_ref().and_then(|side| side.text.as_deref()) else {
            return;
        };
        let Some(right) = self.right.as_ref().and_then(|side| side.text.as_deref()) else {
            return;
        };
        let (rows, stats) = diff::diff_text(left, right, char_level);
        self.text_rows = rows;
        self.text_stats = stats;
    }

    /// 当前看法下有多少行。
    fn row_count(&self) -> usize {
        match self.mode {
            DiffMode::Lines | DiffMode::Chars => self.text_rows.len(),
            DiffMode::Hex => self.hex_rows.len(),
        }
    }

    fn sync_rows(&mut self) {
        let count = self.row_count();
        if self.row_sizes.len() == count {
            return;
        }
        self.row_sizes = Rc::new(vec![size(px(0.), px(ROW_HEIGHT)); count]);
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let moved = self.cursor as i64 + delta as i64;
        self.cursor = moved.clamp(0, count as i64 - 1) as usize;
        self.scroll
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// 状态那一行的字。
    fn status(&self) -> String {
        let mut text = match self.mode {
            DiffMode::Lines | DiffMode::Chars => {
                let stats = self.text_stats;
                format!(
                    "改了 {} 行 · 加了 {} 行 · 少了 {} 行 · 一样 {} 行",
                    stats.changed, stats.added, stats.removed, stats.same
                )
            }
            DiffMode::Hex => {
                let stats = self.hex_stats;
                match stats.first_difference {
                    Some(offset) => format!(
                        "{} 个字节不一样 · 第一处在偏移 {}（0x{offset:x}）",
                        stats.different, offset
                    ),
                    None => "两个文件一模一样".to_string(),
                }
            }
        };
        if let Some(error) = &self.error {
            text.push_str(&format!(" · {error}"));
        }
        text
    }

    fn name_of(path: &std::path::Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }

    // ---- 画 ----

    /// 文本一行：左边一块、右边一块，改过的那段上色。
    fn render_text_row(
        &mut self,
        index: usize,
        row: &TextRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let selected = index == self.cursor;
        // "细到字符"才画行内的高亮；"按行"只标整行。
        let char_level = self.mode == DiffMode::Chars;
        let left_spans = char_level.then_some(row.left_span).flatten();
        let right_spans = char_level.then_some(row.right_span).flatten();
        let changed = row.kind == Kind::Changed;
        let tint = if changed {
            cx.theme().warning
        } else {
            cx.theme().muted_foreground
        };

        h_flex()
            .id(("diff-row", index))
            .test_support()
            .aria_label(format!("{} {} | {}", row.kind.label(), row.left, row.right))
            .w_full()
            .h(px(ROW_HEIGHT))
            .gap_2()
            .items_center()
            .px_2()
            .rounded(radius)
            .bg(if selected {
                cx.theme().muted
            } else {
                transparent_white()
            })
            .child(
                div()
                    .w(px(18.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(tint)
                    .child(row.kind.label().to_string()),
            )
            .child(line_number(row.left_line, muted))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(segments_of(&row.left, left_spans, cx)),
            )
            .child(line_number(row.right_line, muted))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(segments_of(&row.right, right_spans, cx)),
            )
            .into_any_element()
    }

    /// 十六进制一行：偏移 + 左边的字节（附可读部分）+ 右边的字节。
    fn render_hex_row(&mut self, index: usize, row: &HexRow, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let selected = index == self.cursor;
        let mono = cx.theme().mono_font_family.clone();

        h_flex()
            .id(("diff-row", index))
            .test_support()
            .aria_label(format!(
                "{:08x} {} | {}",
                row.offset,
                diff::hex_line(&row.left),
                diff::hex_line(&row.right)
            ))
            .w_full()
            .h(px(ROW_HEIGHT))
            .gap_2()
            .items_center()
            .px_2()
            .rounded(radius)
            .bg(if selected {
                cx.theme().muted
            } else {
                transparent_white()
            })
            .child(
                div()
                    .w(px(60.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .font_family(mono.clone())
                    .child(format!("{:08x}", row.offset)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(mono.clone())
                    .children(segments_of_bytes(&row.left, &row.differs, cx)),
            )
            .child(
                div()
                    .w(px(80.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .font_family(mono.clone())
                    .truncate()
                    .child(diff::ascii_line(&row.left)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(mono.clone())
                    .children(segments_of_bytes(&row.right, &row.differs, cx)),
            )
            .child(
                div()
                    .w(px(80.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .font_family(mono)
                    .truncate()
                    .child(diff::ascii_line(&row.right)),
            )
            .into_any_element()
    }
}

fn line_number(line: Option<usize>, color: gpui_kit::Hsla) -> Div {
    div()
        .w(px(34.))
        .flex_shrink_0()
        .text_xs()
        .text_color(color)
        .text_right()
        .child(line.map_or(String::new(), |line| line.to_string()))
}

/// 把一行文本切成几段并上色（改过的那段用警示色）。
fn segments_of(text: &str, span: Option<(usize, usize)>, cx: &App) -> Vec<AnyElement> {
    let accent = cx.theme().warning;
    diff::text_segments(text, span)
        .into_iter()
        .map(|segment| {
            let mut div = div().whitespace_nowrap().child(segment.text);
            if segment.changed {
                div = div
                    .text_color(accent)
                    .font_weight(gpui_kit::FontWeight::BOLD);
            }
            div.into_any_element()
        })
        .collect()
}

/// 十六进制一行里的几段：不一样的字节用警示色。
fn segments_of_bytes(bytes: &[u8], differs: &[bool], cx: &App) -> Vec<AnyElement> {
    let accent = cx.theme().warning;
    let muted = cx.theme().muted_foreground;
    diff::hex_segments(bytes, differs)
        .into_iter()
        .map(|segment| {
            let mut div = div().whitespace_nowrap().child(segment.text);
            if segment.changed {
                div = div
                    .text_color(accent)
                    .font_weight(gpui_kit::FontWeight::BOLD);
            } else {
                div = div.text_color(muted);
            }
            div.into_any_element()
        })
        .collect()
}

impl Render for DiffPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;
        let status = self.status();
        let row_sizes = self.row_sizes.clone();
        let count = self.row_count();
        let ready = self.left.is_some() && self.right.is_some();

        let modes = self.available_modes();
        let tabs = h_flex()
            .gap_1()
            .children(modes.into_iter().enumerate().map(|(index, mode)| {
                let label = mode.label();
                let tab = Button::new(("diff-tab", index))
                    .label(label)
                    .xsmall()
                    .tooltip(label.to_string());
                let tab = if mode == self.mode { tab } else { tab.ghost() };
                tab.on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
            }));

        let body: AnyElement = if !ready && self.error.is_none() {
            div()
                .id("diff-rows")
                .test_support()
                .aria_label("正在读")
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("正在读这两个文件…")
                .into_any_element()
        } else if count == 0 {
            div()
                .id("diff-rows")
                .test_support()
                .aria_label(status.clone())
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(if self.error.is_some() {
                    "读不出来"
                } else {
                    "两个文件没有差别"
                })
                .into_any_element()
        } else {
            let list = v_virtual_list(
                cx.entity().clone(),
                "rows",
                row_sizes,
                |panel, range, _, cx| {
                    range
                        .filter_map(|index| match panel.mode {
                            DiffMode::Lines | DiffMode::Chars => panel
                                .text_rows
                                .get(index)
                                .cloned()
                                .map(|row| panel.render_text_row(index, &row, cx)),
                            DiffMode::Hex => panel
                                .hex_rows
                                .get(index)
                                .cloned()
                                .map(|row| panel.render_hex_row(index, &row, cx)),
                        })
                        .collect()
                },
            )
            .flex_1()
            .min_h_0();
            div()
                .id("diff-rows")
                .test_support()
                .aria_label(status.clone())
                .flex_1()
                .min_h_0()
                .child(list)
                .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
                .into_any_element()
        };

        let left_name = Self::name_of(&self.left_path);
        let right_name = Self::name_of(&self.right_path);

        div()
            .id("diff")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                cx.stop_propagation();
                match key {
                    "up" => this.move_cursor(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    "enter" => cx.emit(DiffEvent::Reveal(this.left_path.clone())),
                    "escape" => cx.emit(DiffEvent::Close),
                    _ => {}
                }
            }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::Close)))
            .child(
                v_flex()
                    .id("diff-card")
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
                            .child(Icon::new(IconName::FileDiff).size_4().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_semibold()
                                    .truncate()
                                    .child(format!("比较文件：{left_name} ↔ {right_name}")),
                            )
                            .child(
                                Button::new("diff-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .tooltip("关闭")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::Close))),
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
                            .child(tabs)
                            .child(div().flex_1().min_w_0())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("左：{left_name}　右：{right_name}")),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("diff-status")
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
                            .child("↑↓ 翻动")
                            .child("Enter 跳到左边那个文件")
                            .child("Esc 关闭"),
                    ),
            )
    }
}
