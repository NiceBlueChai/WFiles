//! 分析面板：算文件夹多大、找重复文件、算校验和，三样共用一个面板。
//!
//! 算的活都在 [`crate::analyze`]，这里只管三件脏活：
//! 起任务、把结果摆成列表、把 Enter 变成"跳到那个文件"。
//!
//! 三条边界值得记着：
//! - 切页签就是换一趟任务，旧的那趟一定先中止（[`Control::abort`]），
//!   不会有两个线程同时往一份结果里塞东西；
//! - 结果靠"第几趟"（`generation`）认主人，晚到的旧批次一律丢掉；
//! - 面板一被丢掉（关掉）就中止，见 [`Drop`]：不给用户留下
//!   一个看不见还在扫盘的后台线程。

use std::cmp::Reverse;
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
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Render,
    ScrollStrategy, Size, Task, Window, div, px, size, transparent_white, *,
};

use crate::analyze::{
    self, DupGroup, DupOptions, DupStats, SizeEvent, SizeProgress, SizeReport, SumEvent, Sums,
};
use crate::format;
use crate::ops::Control;
use crate::pane::PaneCommand;

const CARD_WIDTH: f32 = 720.;
const CARD_MAX_HEIGHT: f32 = 560.;

/// 目录/组头那一行的高度。
const HEAD_HEIGHT: f32 = 44.;
/// 里面一个文件一行的高度。
const FILE_HEIGHT: f32 = 30.;
/// 校验和一行要放三行摘要，高一点。
const SUM_HEIGHT: f32 = 86.;

/// 面板在算什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalyzeMode {
    /// 这个文件夹多大，谁占得最多。
    Size,
    /// 哪些文件是重复的。
    Duplicates,
    /// 选中文件的校验和。
    Checksum,
}

impl AnalyzeMode {
    pub const ALL: [Self; 3] = [Self::Size, Self::Duplicates, Self::Checksum];

    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "文件夹大小",
            Self::Duplicates => "重复文件",
            Self::Checksum => "校验和",
        }
    }

    /// 打开面板时默认落在哪一页：选中了文件就先看校验和，
    /// 其余（选中目录、或者什么都没选）看大小。
    pub fn default_for(selected: &[PathBuf]) -> Self {
        if selected.iter().any(|path| path.is_file()) {
            Self::Checksum
        } else {
            Self::Size
        }
    }
}

/// 面板告诉窗格的两件事。
#[derive(Clone, Debug)]
pub enum AnalyzeEvent {
    /// 关掉面板（Esc、点遮罩、点关闭按钮）。
    Close,
    /// 关掉面板并跳到这个路径：先切到它所在的目录，再选中它。
    Reveal(PathBuf),
}

/// 列表里的一行。行高、画法、Enter 跳哪儿都由它决定，
/// 三种模式共用一套虚拟列表，于是"↑↓ 走路"只写一遍。
#[derive(Clone, Copy, Debug)]
enum Row {
    /// 一个目录算完了：第几份报告。
    SizeHead(usize),
    /// 那份报告里最大的第几个文件。
    SizeFile { report: usize, file: usize },
    /// 一组重复：第几组。
    DupHead(usize),
    /// 那组里的第几个副本。
    DupFile { group: usize, file: usize },
    /// 一个文件的校验和：第几个。
    Sum(usize),
    /// 一个算不出来的文件：第几个。
    Failure(usize),
}

pub struct AnalyzePanel {
    mode: AnalyzeMode,
    /// 当前目录：没有选中项时算它，也用来显示相对路径。
    dir: PathBuf,
    /// 打开面板那一刻的选中项（按列表顺序）。切页签时还是用这一份，
    /// 免得用户切一下页签、算的东西就变了。
    selected: Vec<PathBuf>,
    sizes: Vec<SizeReport>,
    /// 正在算的那一个目录的进度。
    progress: Option<SizeProgress>,
    groups: Vec<DupGroup>,
    dup_stats: Option<DupStats>,
    sums: Vec<Sums>,
    /// 算不出来的文件与原因。列在最后，别让一个读不了的文件把整趟说成失败。
    failures: Vec<(PathBuf, crate::i18n::Msg)>,
    running: bool,
    /// 后台那一趟的把手。中止就是 `abort()`。
    control: Option<Control>,
    /// 收结果的那个任务；丢掉即放弃。
    task: Option<Task<()>>,
    /// 第几趟。过期的批次靠它认出来。
    generation: usize,
    cursor: usize,
    scroll: VirtualListScrollHandle,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    focus: FocusHandle,
}

impl EventEmitter<AnalyzeEvent> for AnalyzePanel {}

impl Focusable for AnalyzePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for AnalyzePanel {
    fn drop(&mut self) {
        self.stop();
    }
}

impl AnalyzePanel {
    pub fn new(
        dir: PathBuf,
        selected: Vec<PathBuf>,
        mode: AnalyzeMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            mode,
            dir,
            selected,
            sizes: Vec::new(),
            progress: None,
            groups: Vec::new(),
            dup_stats: None,
            sums: Vec::new(),
            failures: Vec::new(),
            running: false,
            control: None,
            task: None,
            generation: 0,
            cursor: 0,
            scroll: VirtualListScrollHandle::new(),
            row_sizes: Rc::new(Vec::new()),
            focus: cx.focus_handle(),
        };
        this.start(cx);
        // 焦点交给窗格去给：面板这一帧才挂上窗口，现在抢不到。
        this
    }

    /// 换一页：等于换一趟任务，旧的先中止。
    ///
    /// 页签上点一下就走这里，所以"算什么"仍然取决于打开时的选中项——
    /// 切页签不改变对象，只改变看法。
    pub fn switch_mode(&mut self, mode: AnalyzeMode, cx: &mut Context<Self>) {
        if mode == self.mode && (self.running || !self.is_empty()) {
            return;
        }
        self.mode = mode;
        self.start(cx);
    }

    fn is_empty(&self) -> bool {
        self.sizes.is_empty() && self.groups.is_empty() && self.sums.is_empty()
    }

    /// 停掉后台那一趟。面板关掉、切页签、用户点"中止"都走这里。
    pub fn stop(&mut self) {
        if let Some(control) = self.control.take() {
            control.abort();
        }
        self.task = None;
        self.running = false;
    }

    /// ↑↓ 挪光标。
    pub fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
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

    /// Enter：跳到光标那一行的路径。
    pub fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.row_at(self.cursor) else {
            return;
        };
        let Some(path) = self.path_of(row) else {
            return;
        };
        cx.emit(AnalyzeEvent::Reveal(path));
    }

    /// 把当前这份结果抄成文本放进剪贴板。
    ///
    /// 算出来的东西最大的用处就是贴到别处去（写进文档、发给同事），
    /// 而面板里那点宽度根本选不全一整行 SHA-256。
    pub fn copy_results(&mut self, cx: &mut Context<Self>) {
        let text = self.as_text();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    // ---- 起任务 ----

    fn start(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.sizes.clear();
        self.progress = None;
        self.groups.clear();
        self.dup_stats = None;
        self.sums.clear();
        self.failures.clear();
        self.cursor = 0;
        self.generation += 1;
        self.running = true;
        self.sync_rows();
        cx.notify();

        let generation = self.generation;
        match self.mode {
            AnalyzeMode::Size => self.start_sizes(generation, cx),
            AnalyzeMode::Duplicates => self.start_duplicates(generation, cx),
            AnalyzeMode::Checksum => self.start_checksums(generation, cx),
        }
    }

    fn start_sizes(&mut self, generation: usize, cx: &mut Context<Self>) {
        let roots = self.size_roots();
        let (tx, rx) = async_channel::unbounded::<SizeEvent>();
        self.control = Some(analyze::spawn_sizes(
            roots,
            Arc::new(move |event| {
                tx.try_send(event).ok();
            }),
        ));
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        match event {
                            SizeEvent::Tick(progress) => this.progress = Some(progress),
                            SizeEvent::Report(report) => {
                                this.sizes.push(report);
                                this.progress = None;
                            }
                            SizeEvent::Finished => {
                                this.running = false;
                                this.control = None;
                            }
                        }
                        this.sync_rows();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn start_duplicates(&mut self, generation: usize, cx: &mut Context<Self>) {
        let options = DupOptions::new(self.dup_root());
        let (tx, rx) = async_channel::unbounded::<analyze::DupEvent>();
        self.control = Some(analyze::spawn_duplicates(
            options,
            Arc::new(move |event| {
                tx.try_send(event).ok();
            }),
        ));
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        match event {
                            analyze::DupEvent::Group(group) => {
                                // 多占得多的组排在前面：用户是来清理的，
                                // 先看到最大的那一组最省事。
                                this.groups.push(group);
                                this.groups.sort_by_key(|group| Reverse(group.wasted()));
                            }
                            analyze::DupEvent::Finished(stats) => {
                                this.dup_stats = Some(stats);
                                this.running = false;
                                this.control = None;
                            }
                        }
                        this.sync_rows();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn start_checksums(&mut self, generation: usize, cx: &mut Context<Self>) {
        let paths = self.sum_files();
        let (tx, rx) = async_channel::unbounded::<SumEvent>();
        self.control = Some(analyze::spawn_checksums(
            paths,
            Arc::new(move |event| {
                tx.try_send(event).ok();
            }),
        ));
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let mut cx = cx.clone();
                let alive = this
                    .update(&mut cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        match event {
                            SumEvent::File(sums) => this.sums.push(sums),
                            SumEvent::Failed { path, error } => this.failures.push((path, error)),
                            SumEvent::Finished => {
                                this.running = false;
                                this.control = None;
                            }
                        }
                        this.sync_rows();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    // ---- 算什么 ----

    /// 大小：选中的目录；一个目录都没选中就算当前目录。
    fn size_roots(&self) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = self
            .selected
            .iter()
            .filter(|path| path.is_dir())
            .cloned()
            .collect();
        if dirs.is_empty() {
            vec![self.dir.clone()]
        } else {
            dirs
        }
    }

    /// 重复：从选中的第一个目录（没有就从当前目录）往下找。
    fn dup_root(&self) -> PathBuf {
        self.selected
            .iter()
            .find(|path| path.is_dir())
            .cloned()
            .unwrap_or_else(|| self.dir.clone())
    }

    /// 校验和：选中的文件；目录不算（它的"校验和"没有意义）。
    fn sum_files(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter(|path| path.is_file())
            .cloned()
            .collect()
    }

    // ---- 行 ----

    fn row_count(&self) -> usize {
        match self.mode {
            AnalyzeMode::Size => self
                .sizes
                .iter()
                .map(|report| 1 + report.largest.len())
                .sum(),
            AnalyzeMode::Duplicates => self.groups.iter().map(|group| 1 + group.paths.len()).sum(),
            AnalyzeMode::Checksum => self.sums.len() + self.failures.len(),
        }
    }

    fn row_at(&self, index: usize) -> Option<Row> {
        let mut left = index;
        match self.mode {
            AnalyzeMode::Size => {
                for (report, size) in self.sizes.iter().enumerate() {
                    if left == 0 {
                        return Some(Row::SizeHead(report));
                    }
                    left -= 1;
                    if left < size.largest.len() {
                        return Some(Row::SizeFile { report, file: left });
                    }
                    left -= size.largest.len();
                }
                None
            }
            AnalyzeMode::Duplicates => {
                for (group, entry) in self.groups.iter().enumerate() {
                    if left == 0 {
                        return Some(Row::DupHead(group));
                    }
                    left -= 1;
                    if left < entry.paths.len() {
                        return Some(Row::DupFile { group, file: left });
                    }
                    left -= entry.paths.len();
                }
                None
            }
            AnalyzeMode::Checksum => {
                if left < self.sums.len() {
                    return Some(Row::Sum(left));
                }
                left -= self.sums.len();
                if left < self.failures.len() {
                    return Some(Row::Failure(left));
                }
                None
            }
        }
    }

    /// 这一行有多高。虚拟列表要逐行报高，所以行种决定高度。
    fn row_height(&self, row: Row) -> f32 {
        match row {
            Row::SizeHead(_) => HEAD_HEIGHT,
            Row::SizeFile { .. } => FILE_HEIGHT,
            Row::DupHead(_) => HEAD_HEIGHT,
            Row::DupFile { .. } => FILE_HEIGHT,
            Row::Sum(_) => SUM_HEIGHT,
            Row::Failure(_) => FILE_HEIGHT,
        }
    }

    /// 这一行 Enter 跳哪儿。
    fn path_of(&self, row: Row) -> Option<PathBuf> {
        match row {
            Row::SizeHead(report) => self.sizes.get(report).map(|it| it.root.clone()),
            Row::SizeFile { report, file } => self
                .sizes
                .get(report)
                .and_then(|it| it.largest.get(file))
                .map(|it| it.path.clone()),
            Row::DupHead(group) => self
                .groups
                .get(group)
                .and_then(|it| it.paths.first())
                .cloned(),
            Row::DupFile { group, file } => self
                .groups
                .get(group)
                .and_then(|it| it.paths.get(file))
                .cloned(),
            Row::Sum(index) => self.sums.get(index).map(|it| it.path.clone()),
            Row::Failure(index) => self.failures.get(index).map(|it| it.0.clone()),
        }
    }

    fn sync_rows(&mut self) {
        // 行种决定了行高，而同一行数下也可能是不同的行种（切页签、失败项插进来），
        // 所以每次都重排：几百行的 `Vec` 不值得为它记一套脏标记。
        let count = self.row_count();
        let mut heights = Vec::with_capacity(count);
        for index in 0..count {
            let height = self
                .row_at(index)
                .map_or(FILE_HEIGHT, |row| self.row_height(row));
            heights.push(size(px(0.), px(height)));
        }
        self.row_sizes = Rc::new(heights);
    }

    // ---- 文案 ----

    /// 结果区上面那一行字：正在算 / 算完了 / 什么都没有。
    fn status(&self) -> String {
        match self.mode {
            AnalyzeMode::Size => {
                let done: u64 = self.sizes.iter().map(|it| it.bytes).sum();
                let files: usize = self.sizes.iter().map(|it| it.files).sum();
                let mut text = if self.running {
                    match &self.progress {
                        Some(progress) => format!(
                            "正在算… 已扫 {} 个文件 · {}",
                            progress.files,
                            format::file_size(progress.bytes)
                        ),
                        None => "正在算…".to_string(),
                    }
                } else if self.sizes.is_empty() {
                    "这个目录里没有文件".to_string()
                } else {
                    format!(
                        "{} 个目录 · 共 {} · {} 个文件",
                        self.sizes.len(),
                        format::file_size(done),
                        files
                    )
                };
                if self.sizes.iter().any(|it| it.cancelled) {
                    text.push_str(" · 中止了，这是半截的数");
                }
                text
            }
            AnalyzeMode::Duplicates => {
                let wasted: u64 = self.groups.iter().map(|it| it.wasted()).sum();
                if self.running {
                    format!("正在找… 已找到 {} 组", self.groups.len())
                } else if self.groups.is_empty() {
                    "没有找到内容一样的文件".to_string()
                } else {
                    format!(
                        "找到 {} 组 · 多占 {} · 比对了 {} 个文件",
                        self.groups.len(),
                        format::file_size(wasted),
                        self.dup_stats.map_or(0, |it| it.scanned)
                    )
                }
            }
            AnalyzeMode::Checksum => {
                let mut text = if self.running {
                    format!("正在算… 已算完 {}", self.sums.len())
                } else if self.sums.is_empty() {
                    "没有可以算的文件".to_string()
                } else {
                    format!("算完 {} 个文件", self.sums.len())
                };
                if !self.failures.is_empty() {
                    text.push_str(&format!(" · {} 个算不出来", self.failures.len()));
                }
                text
            }
        }
    }

    /// 复制出去的文本：面板里看不全的东西（一整行 SHA-256）得能原样带走。
    fn as_text(&self) -> String {
        let mut out = String::new();
        match self.mode {
            AnalyzeMode::Size => {
                for report in &self.sizes {
                    out.push_str(&format!(
                        "{}：{}（{} 个文件，{} 个目录）\n",
                        report.root.display(),
                        format::file_size(report.bytes),
                        report.files,
                        report.dirs
                    ));
                    for file in &report.largest {
                        out.push_str(&format!(
                            "  {} · {}\n",
                            file.path.display(),
                            format::file_size(file.bytes)
                        ));
                    }
                }
            }
            AnalyzeMode::Duplicates => {
                for group in &self.groups {
                    out.push_str(&format!(
                        "{} 个副本 · 每个 {} · 多占 {}\n",
                        group.paths.len(),
                        format::file_size(group.bytes),
                        format::file_size(group.wasted())
                    ));
                    for path in &group.paths {
                        out.push_str(&format!("  {}\n", path.display()));
                    }
                }
            }
            AnalyzeMode::Checksum => {
                for sums in &self.sums {
                    out.push_str(&format!(
                        "{}  {}   MD5 {}   SHA-1 {}   SHA-256 {}\n",
                        sums.path.display(),
                        format::file_size(sums.bytes),
                        sums.md5,
                        sums.sha1,
                        sums.sha256
                    ));
                }
                for (path, error) in &self.failures {
                    out.push_str(&format!("{}  {}\n", path.display(), error));
                }
            }
        }
        out
    }

    /// 相对当前目录的路径：同一个目录下的几个副本靠它分得清。
    fn short_path(&self, path: &std::path::Path) -> String {
        path.strip_prefix(&self.dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn name_of(path: &std::path::Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }

    // ---- 画 ----

    fn render_row(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = self.row_at(index)?;
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let selected = index == self.cursor;
        let height = self.row_height(row);

        let mut line = h_flex()
            .id(("analyze-row", index))
            .test_support()
            .w_full()
            .h(px(height))
            .gap_2()
            .items_center()
            .px_3()
            .rounded(radius)
            .cursor_pointer()
            .bg(if selected {
                cx.theme().muted
            } else {
                transparent_white()
            });

        match row {
            Row::SizeHead(report) => {
                let report = self.sizes.get(report).cloned()?;
                let label = Self::name_of(&report.root);
                let detail = format!(
                    "{} · {} 个文件 · {} 个目录",
                    format::file_size(report.bytes),
                    report.files,
                    report.dirs
                );
                line = line
                    .child(Icon::new(IconName::Folder))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(label.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(detail.clone()),
                            ),
                    )
                    .aria_label(format!("{label} {detail}"));
            }
            Row::SizeFile { report, file } => {
                let entry = self
                    .sizes
                    .get(report)
                    .and_then(|it| it.largest.get(file))
                    .cloned()?;
                let label = Self::name_of(&entry.path);
                let detail = format::file_size(entry.bytes);
                line = line.child(div().text_xs().text_color(muted).w(px(24.)).child("·"));
                line = line
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(label.clone()),
                    )
                    .child(div().text_xs().text_color(muted).child(detail.clone()))
                    .aria_label(format!("{label} {detail}"));
            }
            Row::DupHead(group) => {
                let group = self.groups.get(group).cloned()?;
                let label = format!(
                    "{} 个副本 · 每个 {} · 多占 {}",
                    group.paths.len(),
                    format::file_size(group.bytes),
                    format::file_size(group.wasted())
                );
                let digest: String = group.digest.chars().take(16).collect();
                line = line
                    .child(Icon::new(IconName::Copy))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(digest),
                    )
                    .aria_label(label);
            }
            Row::DupFile { group, file } => {
                let path = self
                    .groups
                    .get(group)
                    .and_then(|it| it.paths.get(file))
                    .cloned()?;
                let label = self.short_path(&path);
                line = line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(muted)
                        .truncate()
                        .child(label.clone()),
                );
                line = line.aria_label(label);
            }
            Row::Sum(index) => {
                let sums = self.sums.get(index).cloned()?;
                let label = Self::name_of(&sums.path);
                let mono = cx.theme().mono_font_family.clone();
                line = line.child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0()
                        .child(div().text_sm().truncate().child(format!(
                            "{} · {}",
                            label,
                            format::file_size(sums.bytes)
                        )))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .font_family(mono.clone())
                                .truncate()
                                .child(format!("MD5      {}", sums.md5)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .font_family(mono.clone())
                                .truncate()
                                .child(format!("SHA-1    {}", sums.sha1)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .font_family(mono)
                                .truncate()
                                .child(format!("SHA-256  {}", sums.sha256)),
                        ),
                );
                line = line.aria_label(format!("{label} {}", sums.sha256));
            }
            Row::Failure(index) => {
                let (path, error) = self.failures.get(index).cloned()?;
                let label = Self::name_of(&path);
                line = line.child(Icon::new(IconName::Info).text_color(muted));
                line = line
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(muted)
                            .truncate()
                            .child(format!("{label} · {error}")),
                    )
                    .aria_label(format!("{label} {error}"));
            }
        }

        Some(
            line.on_click(cx.listener(move |this, _, _, cx| {
                this.cursor = index;
                this.reveal_cursor(cx);
            }))
            .into_any_element(),
        )
    }
}

impl Render for AnalyzePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let radius = cx.theme().radius;
        let status = self.status();
        let row_sizes = self.row_sizes.clone();
        let count = self.row_count();

        // 页签：三个入口共用一个面板，切一下就换一趟任务。
        let tabs = h_flex()
            .id("analyze-tabs")
            .test_support()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .children(AnalyzeMode::ALL.iter().enumerate().map(|(index, mode)| {
                let mode = *mode;
                let active = mode == self.mode;
                let label = mode.label();
                let tab = Button::new(("analyze-tab", index))
                    .label(label)
                    .small()
                    .tooltip(label.to_string());
                if active { tab } else { tab.ghost() }
                    .on_click(cx.listener(move |this, _, _, cx| this.switch_mode(mode, cx)))
            }));

        let body: AnyElement = if count == 0 {
            div()
                .id("analyze-rows")
                .test_support()
                .aria_label(if self.running {
                    "正在算"
                } else {
                    status.as_str()
                })
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(if self.running {
                    "正在算…"
                } else {
                    "没有结果"
                })
                .into_any_element()
        } else {
            let list = v_virtual_list(
                cx.entity().clone(),
                "rows",
                row_sizes,
                |panel, range, _, cx| {
                    range
                        .filter_map(|index| panel.render_row(index, cx))
                        .collect()
                },
            )
            .flex_1()
            .min_h_0();
            div()
                .id("analyze-rows")
                .test_support()
                .aria_label(status.clone())
                .flex_1()
                .min_h_0()
                .child(list)
                .scrollbar(&self.scroll, ScrollbarAxis::Vertical)
                .into_any_element()
        };

        div()
            .id("analyze")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .track_focus(&self.focus)
            // 键盘走遮罩自己的 `on_key_down`（冒泡阶段）：面板里有焦点时，
            // 窗格那套带上下文的快捷键根本匹配不上（遮罩没有列表的 key context），
            // 于是这里就是唯一收得到键的地方——↑↓/Enter/Esc 与开关键都得自己认。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                // 开关面板那个键按一下等于收起：焦点在面板里时它到不了窗格的动作，
                // 所以在这里自己认下来（焦点在列表上时由窗格那条路处理）。
                let toggle = crate::keymap::KeyMap::read(cx).key_for(PaneCommand::Analyze);
                if !toggle.is_empty() && event.keystroke.unparse() == toggle {
                    cx.stop_propagation();
                    cx.emit(AnalyzeEvent::Close);
                    return;
                }
                let key = event.keystroke.key.as_str();
                cx.stop_propagation();
                match key {
                    "up" => this.move_cursor(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    "enter" => this.reveal_cursor(cx),
                    "escape" => cx.emit(AnalyzeEvent::Close),
                    _ => {}
                }
            }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(AnalyzeEvent::Close)))
            .child(
                v_flex()
                    .id("analyze-card")
                    .test_support()
                    .w(px(CARD_WIDTH))
                    .max_h(px(CARD_MAX_HEIGHT))
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .bg(cx.theme().background)
                    // 点卡片里不该关掉面板：吃掉点击，别让它冒到遮罩上。
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(46.))
                            .border_b_1()
                            .border_color(border)
                            .child(Icon::new(IconName::ChartPie).size_4().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_semibold()
                                    .truncate()
                                    .child(self.mode.label().to_string()),
                            )
                            .child(
                                Button::new("analyze-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .tooltip("关闭")
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(AnalyzeEvent::Close)),
                                    ),
                            ),
                    )
                    .child(tabs)
                    .child(
                        h_flex()
                            .id("analyze-status")
                            .test_support()
                            .aria_label(status.clone())
                            .px_3()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(muted)
                            .child(div().flex_1().min_w_0().truncate().child(status))
                            .child(
                                Button::new("analyze-copy")
                                    .label("复制结果")
                                    .small()
                                    .ghost()
                                    .tooltip("把这份结果抄成文本放进剪贴板")
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_results(cx))),
                            )
                            .child(if self.running {
                                Button::new("analyze-stop")
                                    .label("中止")
                                    .small()
                                    .ghost()
                                    .tooltip("停下这一趟，已经算出来的那部分留着")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.stop();
                                        cx.notify();
                                    }))
                            } else {
                                Button::new("analyze-rerun")
                                    .label("重算")
                                    .small()
                                    .ghost()
                                    .tooltip("重新扫一遍")
                                    .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
                            }),
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
                            .child("Enter 跳到该文件")
                            .child("Esc 关闭"),
                    ),
            )
    }
}
