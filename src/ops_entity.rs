//! 文件操作的 UI 侧枢纽：剪贴板、撤销栈、进行中的任务与待处理的对话框。
//!
//! 窗格只负责发起请求（“我想删这几个”），真正动手、汇报进度、
//! 弹确认与冲突选择都在这里；完成后广播 [`OpsEvent`]，各窗格自己对照路径刷新。

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use gpui_kit::{AsyncApp, WeakEntity, *};

use std::collections::HashMap;
use std::sync::Arc;

use crate::bulk_rename::{self, BulkRename, BulkRenamePreview};
use crate::ops::{
    self, ConflictPolicy, Control, ControlState, History, Outcome, Progress, RemoteSides,
};
use crate::remote::{self, Dav};
use crate::remotes::Remotes;

/// 广播给窗格的事件：文件系统变了，该刷新的自己对照路径判断。
#[derive(Clone, Debug)]
pub enum OpsEvent {
    /// 这些路径消失了、出现了，或者被改写了。
    Changed {
        removed: Vec<PathBuf>,
        changed: Vec<PathBuf>,
        /// 重命名/移动的“旧 -> 新”，窗格的目录被搬走时靠它重新定位。
        renamed_to: Vec<(PathBuf, PathBuf)>,
    },
    /// 剪贴板变化（粘贴按钮的可用性）。
    ClipboardChanged,
    /// 输入对话框开/关，应用层据此创建/销毁输入框。
    PromptChanged { open: bool },
    /// 批量重命名对话框开/关，应用层据此创建/销毁那一组输入框。
    BulkChanged { open: bool },
}

impl EventEmitter<OpsEvent> for Ops {}

/// 剪贴板：拷贝/剪切暂存。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    /// true = 剪切（粘贴后删源）。
    pub cut: bool,
}

/// 待确认的破坏性操作。
#[derive(Clone, Debug)]
pub struct Confirm {
    pub title: String,
    pub message: String,
    /// 确认按钮的文案，删除类是“移到回收站”。
    pub ok_label: String,
    pub ok_dangerous: bool,
    pub action: PendingAction,
}

#[derive(Clone, Debug)]
pub enum PendingAction {
    /// 把选中的路径移进回收站。
    Trash { paths: Vec<PathBuf> },
    /// 撤销上一步操作。
    Undo,
    /// 新建文件夹（输入名字后）。
    CreateFolder { parent: PathBuf },
    /// 重命名。
    Rename { path: PathBuf },
}

/// 粘贴前的冲突预检：目标目录里已经有同名项，先问怎么办。
#[derive(Clone, Debug)]
pub struct Conflict {
    pub sources: Vec<PathBuf>,
    pub dest: PathBuf,
    pub cut: bool,
    /// 名字冲突的数量，展示用。
    pub collisions: usize,
}

/// 待填写的输入对话框（新建文件夹、重命名）。
#[derive(Clone, Debug)]
pub struct Prompt {
    pub title: String,
    pub label: String,
    pub initial: String,
    /// 上一次提交被拒的原因，显示在输入框下方。
    pub error: Option<String>,
    pub action: PendingAction,
}

/// 批量重命名对话框：一套规则 + 实时算出来的预览。
#[derive(Clone, Debug)]
pub struct BulkState {
    pub sources: Vec<PathBuf>,
    pub rule: BulkRename,
    pub previews: Vec<BulkRenamePreview>,
}

/// 进行中的任务。
#[derive(Clone, Debug)]
pub struct Running {
    pub label: String,
    pub done: usize,
    pub total: usize,
    /// 已经搬过去的字节数。删除、重命名这类不按字节算的操作是 0。
    pub bytes_done: u64,
    /// 这次总共要搬多少字节；数不出来是 0，界面退化成只看条目数。
    pub bytes_total: u64,
    pub state: ControlState,
    /// 只有拷贝/移动能中途停下：它们是一个数据块一个数据块写的，
    /// 每块之间会看一眼控制手柄。删除和重命名是一口气跑完的短活，
    /// 给了按钮点了也没反应，不如不给。
    pub can_pause: bool,
    started: Instant,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl Running {
    fn new(label: String, total: usize, can_pause: bool) -> Self {
        Self {
            label,
            done: 0,
            total: total.max(1),
            bytes_done: 0,
            bytes_total: 0,
            state: ControlState::Running,
            can_pause,
            started: Instant::now(),
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    fn mark_paused(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
        }
        self.state = ControlState::Paused;
    }

    fn mark_resumed(&mut self) {
        if let Some(at) = self.paused_at.take() {
            self.paused_total += at.elapsed();
        }
        self.state = ControlState::Running;
    }

    /// 真正干活的时长：暂停那一段不算。
    /// 不扣掉的话，暂停五分钟再继续，速度会显示成几乎为零。
    fn active(&self) -> Duration {
        let paused_now = self.paused_at.map(|at| at.elapsed()).unwrap_or_default();
        self.started
            .elapsed()
            .saturating_sub(self.paused_total)
            .saturating_sub(paused_now)
    }

    /// 进度条的比例。有总字节数就按字节走——拷一个 2 GB 的文件时
    /// "3/4" 这种条目进度没什么用；数不出字节才退回按条目数。
    pub fn fraction(&self) -> f32 {
        let ratio = if self.bytes_total > 0 {
            self.bytes_done as f32 / self.bytes_total as f32
        } else if self.total > 0 {
            self.done as f32 / self.total as f32
        } else {
            0.
        };
        ratio.clamp(0., 1.)
    }

    /// 每秒字节数。开头半秒不报：那会儿除出来的数抖动得没法看。
    pub fn bytes_per_second(&self) -> Option<u64> {
        let elapsed = self.active();
        if elapsed < Duration::from_millis(400) || self.bytes_done == 0 {
            return None;
        }
        let rate = self.bytes_done as f64 / elapsed.as_secs_f64();
        if !rate.is_finite() {
            return None;
        }
        Some(rate as u64)
    }

    /// 还剩多久。速度还没出来、或者总字节数未知，就算不出来。
    pub fn remaining(&self) -> Option<Duration> {
        let rate = self.bytes_per_second()?;
        let left = self.bytes_total.checked_sub(self.bytes_done)?;
        if rate == 0 || left == 0 {
            return Some(Duration::ZERO);
        }
        Some(Duration::from_secs_f64(left as f64 / rate as f64))
    }
}

/// 排队等待的任务。
struct Job {
    label: String,
    total: usize,
    can_pause: bool,
    /// 真正干活的闭包：领走一个控制手柄和一条回传通道。
    run: Box<dyn FnOnce(Control, async_channel::Sender<WorkerMessage>) + Send>,
}

/// 上一次搬运里没成功的项，供"重试"用。
struct Retry {
    sources: Vec<PathBuf>,
    dest: PathBuf,
    cut: bool,
    policy: ConflictPolicy,
}

/// 后台线程送回的消息。
enum WorkerMessage {
    Progress(Progress),
    Done(JobResult),
}

enum JobResult {
    /// 移进回收站的结果。
    Trashed(Vec<(PathBuf, SystemTime)>),
    /// 拷贝/移动的结果。
    Transferred {
        outcome: Outcome,
        cut: bool,
        /// 用户中途叫停。中止不算失败，也不进重试列表。
        aborted: bool,
        /// 重试要用的原始参数：目标目录、是不是剪切、冲突策略，
        /// 源用失败项自己那一份（成功的不用重来）。
        dest: PathBuf,
        policy: ConflictPolicy,
    },
    /// 批量重命名的结果。
    BulkRenamed {
        renamed: Vec<(PathBuf, PathBuf)>,
        failed: Vec<(PathBuf, String)>,
    },
    /// 粘贴前的冲突预检结果：目标在远程时，判重得问服务器，
    /// 所以这一趟也跑在后台线程上。
    Preflight {
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        collisions: usize,
    },
    /// 撤销的结果。撤销栈整条被搬进后台线程又搬回来：
    /// 撤不动时要能把记录放回栈里，所以不能只搬栈顶。
    Undone {
        history: History,
        involved: Vec<PathBuf>,
        result: Result<String, String>,
    },
}

pub struct Ops {
    clipboard: Clipboard,
    history: History,
    running: Option<Running>,
    /// 当前任务的控制手柄；队列里等着的还没有。
    control: Option<Control>,
    /// 排队等着的后续任务。文件操作一次只跑一个，
    /// 免得两个拷贝抢同一块磁盘、也免得进度条打架。
    queue: VecDeque<Job>,
    /// 上一次搬运里失败的项，有它才显示"重试"。
    retry: Option<Retry>,
    confirm: Option<Confirm>,
    conflict: Option<Conflict>,
    prompt: Option<Prompt>,
    bulk: Option<BulkState>,
    /// 最近一次操作的结论，显示在底部状态栏。
    last_message: Option<String>,
    /// 远程连接注册表：只在主线程上用——取客户端，然后把客户端搬进后台线程。
    remotes: Option<Entity<Remotes>>,
    /// 上一条消息是不是错误（决定显示颜色）。
    last_message_is_error: bool,
}

impl Ops {
    /// `remotes` 用来在搬运前取远程客户端；没有远程连接（或测试里）传 `None`。
    pub fn new(remotes: Option<Entity<Remotes>>) -> Self {
        Self {
            remotes,
            clipboard: Clipboard::default(),
            history: History::default(),
            running: None,
            control: None,
            queue: VecDeque::new(),
            retry: None,
            confirm: None,
            conflict: None,
            prompt: None,
            bulk: None,
            last_message: None,
            last_message_is_error: false,
        }
    }

    // ---- 状态查询（渲染用）----

    pub fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    pub fn running(&self) -> Option<&Running> {
        self.running.as_ref()
    }

    /// 后面还排着几个任务。
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// “重试”按钮的文案；没有失败项时返回 None。
    pub fn retry_label(&self) -> Option<String> {
        let retry = self.retry.as_ref()?;
        Some(format!("重试 {} 项", retry.sources.len()))
    }

    // ---- 任务控制：暂停 / 继续 / 中止 / 重试 ----

    /// 暂停。工作线程会在下一个数据块边界停下，
    /// 所以界面上标成"已暂停"会比实际停下的时刻早一点点——
    /// 一个数据块是 1 MiB，这个误差看不出来。
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if let Some(control) = self.control.as_ref() {
            control.pause();
        }
        if let Some(running) = self.running.as_mut() {
            running.mark_paused();
        }
        cx.notify();
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if let Some(control) = self.control.as_ref() {
            control.resume();
        }
        if let Some(running) = self.running.as_mut() {
            running.mark_resumed();
        }
        cx.notify();
    }

    /// 中止。已经拷过去的项留着，正在拷的那一个会被删掉（不留半成品）。
    pub fn abort(&mut self, cx: &mut Context<Self>) {
        if let Some(control) = self.control.as_ref() {
            control.abort();
        }
        if let Some(running) = self.running.as_mut() {
            running.state = ControlState::Aborting;
        }
        self.last_message = Some("正在中止…".to_string());
        self.last_message_is_error = false;
        cx.notify();
    }

    /// 重跑上次失败的那些项。
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(retry) = self.retry.take() else {
            return;
        };
        cx.notify();
        self.start_transfer(retry.sources, retry.dest, retry.cut, retry.policy, cx);
    }

    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    pub fn conflict(&self) -> Option<&Conflict> {
        self.conflict.as_ref()
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    pub fn bulk(&self) -> Option<&BulkState> {
        self.bulk.as_ref()
    }

    /// “撤销”按钮的文案；没有可撤销操作时返回 None。
    pub fn undo_label(&self) -> Option<String> {
        self.history.last_describe()
    }

    pub fn last_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    pub fn last_message_is_error(&self) -> bool {
        self.last_message_is_error
    }

    // ---- 发起请求 ----

    /// 新建文件夹：不直接动手，先问名字（预填一个没被占用的“未命名文件夹”）。
    pub fn request_new_folder(&mut self, parent: PathBuf, cx: &mut Context<Self>) {
        let initial = ops::fresh_folder_path(&parent)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名文件夹".to_string());
        self.prompt = Some(Prompt {
            title: "新建文件夹".to_string(),
            label: "名称".to_string(),
            initial,
            error: None,
            action: PendingAction::CreateFolder { parent },
        });
        cx.emit(OpsEvent::PromptChanged { open: true });
        cx.notify();
    }

    /// 重命名：预填当前名字。
    pub fn request_rename(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let initial = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.prompt = Some(Prompt {
            title: "重命名".to_string(),
            label: "新名称".to_string(),
            initial,
            error: None,
            action: PendingAction::Rename { path },
        });
        cx.emit(OpsEvent::PromptChanged { open: true });
        cx.notify();
    }

    /// 移到回收站：破坏性操作，先确认。
    pub fn request_trash(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        self.confirm = Some(Confirm {
            title: "移到回收站".to_string(),
            message: format!("确定把 {count} 个项目移到回收站吗？之后还可以撤销。"),
            ok_label: "移到回收站".to_string(),
            ok_dangerous: true,
            action: PendingAction::Trash { paths },
        });
        cx.notify();
    }

    pub fn copy(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.set_clipboard(paths, false, cx);
    }

    pub fn cut(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.set_clipboard(paths, true, cx);
    }

    fn set_clipboard(&mut self, paths: Vec<PathBuf>, cut: bool, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.clipboard = Clipboard { paths, cut };
        self.last_message = Some(if cut {
            format!("已剪切 {} 项，到目标文件夹粘贴", self.clipboard.paths.len())
        } else {
            format!("已复制 {} 项，到目标文件夹粘贴", self.clipboard.paths.len())
        });
        self.last_message_is_error = false;
        cx.notify();
        cx.emit(OpsEvent::ClipboardChanged);
    }

    /// 粘贴：先预检冲突，有冲突先问，没有就直接开始。
    pub fn request_paste(&mut self, dest: PathBuf, cx: &mut Context<Self>) {
        if self.clipboard.paths.is_empty() {
            return;
        }
        let sources = self.clipboard.paths.clone();
        let cut = self.clipboard.cut;
        self.begin_transfer(sources, dest, cut, cx);
    }

    /// 拖放落下去：和粘贴同一条链路（同样先预检冲突），
    /// 但**不动剪贴板**——用户剪贴板里放着待粘的东西时，
    /// 拖一次就把那批待粘的顶掉，是最难解释的一类数据丢失。
    pub fn drop_into(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        self.begin_transfer(sources, dest, cut, cx);
    }

    pub fn request_undo(&mut self, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            self.last_message = Some("没有可撤销的操作".to_string());
            self.last_message_is_error = false;
            cx.notify();
            return;
        }
        let describe = self
            .history
            .last_describe()
            .unwrap_or_else(|| "上一步操作".to_string());
        self.confirm = Some(Confirm {
            title: "撤销".to_string(),
            message: format!("确定撤销“{describe}”吗？"),
            ok_label: "撤销".to_string(),
            ok_dangerous: false,
            action: PendingAction::Undo,
        });
        cx.notify();
    }

    // ---- 批量重命名 ----

    /// 打开批量重命名对话框：默认规则是“什么都不改”，预览逐项算出来。
    pub fn request_bulk_rename(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.len() < 2 {
            return;
        }
        let rule = BulkRename::default();
        let previews = bulk_rename::preview(&paths, &rule);
        self.bulk = Some(BulkState {
            sources: paths,
            rule,
            previews,
        });
        cx.emit(OpsEvent::BulkChanged { open: true });
        cx.notify();
    }

    /// 规则变了：重算预览。预览是纯函数，改一个字符全部项立刻跟着变。
    pub fn set_bulk_rule(&mut self, rule: BulkRename, cx: &mut Context<Self>) {
        if let Some(bulk) = self.bulk.as_mut() {
            bulk.previews = bulk_rename::preview(&bulk.sources, &rule);
            bulk.rule = rule;
        }
        cx.notify();
    }

    pub fn dismiss_bulk(&mut self, cx: &mut Context<Self>) {
        self.bulk = None;
        cx.emit(OpsEvent::BulkChanged { open: false });
        cx.notify();
    }

    /// 执行：只动可执行的项，结果压进撤销栈（整批算一条记录，一次撤销全改回）。
    pub fn apply_bulk_rename(&mut self, cx: &mut Context<Self>) {
        let Some(bulk) = self.bulk.clone() else {
            return;
        };
        let count = bulk_rename::executable_count(&bulk.previews);
        if count == 0 {
            self.last_message = Some("没有可执行的重命名".to_string());
            self.last_message_is_error = true;
            cx.notify();
            return;
        }

        self.bulk = None;
        cx.emit(OpsEvent::BulkChanged { open: false });
        self.enqueue(
            Job {
                label: format!("正在批量重命名 {count} 项"),
                total: count,
                can_pause: false,
                run: Box::new(move |_control, tx| {
                    let (renamed, failed) = bulk_rename::apply(&bulk.previews);
                    let _ = tx.try_send(WorkerMessage::Done(JobResult::BulkRenamed {
                        renamed,
                        failed,
                    }));
                }),
            },
            cx,
        );
    }

    // ---- 对话框回调 ----

    pub fn confirm_ok(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        cx.notify();

        match confirm.action {
            PendingAction::Trash { paths } => self.start_trash(paths, cx),
            PendingAction::Undo => self.run_undo(cx),
            // 新建/重命名走输入对话框，不会出现在确认框里；留着兜底。
            PendingAction::CreateFolder { parent } => {
                let name = "未命名文件夹".to_string();
                self.finish_create_folder(parent, name, cx)
            }
            PendingAction::Rename { path } => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.finish_rename(path, name, cx)
            }
        }
    }

    pub fn confirm_cancel(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    /// 输入框提交：校验并执行；被拒时把原因写回 prompt，对话框留着。
    pub fn submit_prompt(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.clone() else {
            return;
        };
        match prompt.action {
            PendingAction::CreateFolder { parent, .. } => {
                self.finish_create_folder(parent, value, cx)
            }
            PendingAction::Rename { path, .. } => self.finish_rename(path, value, cx),
            _ => {}
        }
    }

    pub fn dismiss_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt = None;
        cx.emit(OpsEvent::PromptChanged { open: false });
        cx.notify();
    }

    /// 冲突对话框的三选一（应用到本次粘贴的剩余全部）。
    pub fn resolve_conflict(&mut self, policy: ConflictPolicy, cx: &mut Context<Self>) {
        let Some(conflict) = self.conflict.take() else {
            return;
        };
        cx.notify();
        self.start_transfer(conflict.sources, conflict.dest, conflict.cut, policy, cx);
    }

    pub fn cancel_conflict(&mut self, cx: &mut Context<Self>) {
        self.conflict = None;
        self.last_message = Some("已取消粘贴".to_string());
        self.last_message_is_error = false;
        cx.notify();
    }

    // ---- 执行 ----

    fn finish_create_folder(&mut self, parent: PathBuf, name: String, cx: &mut Context<Self>) {
        match ops::create_folder_named(&parent, &name) {
            Ok(path) => {
                self.prompt = None;
                self.history.push(ops::HistoryEntry::Created { path: path.clone() });
                self.last_message = Some(format!("已创建“{}”", display_name(&path)));
                self.last_message_is_error = false;
                cx.emit(OpsEvent::PromptChanged { open: false });
                cx.notify();
                cx.emit(OpsEvent::Changed {
                    removed: Vec::new(),
                    changed: vec![path],
                    renamed_to: Vec::new(),
                });
            }
            Err(error) => self.reject_prompt(error, cx),
        }
    }

    fn finish_rename(&mut self, path: PathBuf, name: String, cx: &mut Context<Self>) {
        match ops::rename(&path, &name) {
            Ok(renamed) => {
                // 先把更早的记录指向新路径，再压入本次记录，避免改到自己头上。
                self.history.note_renamed(&path, &renamed);
                self.prompt = None;
                self.history
                    .push(ops::HistoryEntry::Renamed {
                        from: path.clone(),
                        to: renamed.clone(),
                    });
                self.last_message = Some(format!("已重命名为“{}”", display_name(&renamed)));
                self.last_message_is_error = false;
                cx.emit(OpsEvent::PromptChanged { open: false });
                cx.notify();
                cx.emit(OpsEvent::Changed {
                    removed: vec![path.clone()],
                    changed: vec![renamed.clone()],
                    renamed_to: vec![(path, renamed)],
                });
            }
            Err(error) => self.reject_prompt(error, cx),
        }
    }

    /// 输入不合法：把原因写进 prompt 自己，对话框不关。
    fn reject_prompt(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.error = Some(error);
        }
        cx.notify();
    }

    fn run_undo(&mut self, cx: &mut Context<Self>) {
        // 先取出栈顶涉及的路径（撤销前后都要刷新），再执行。
        let involved = self.history.last_paths();
        // 客户端要在主线程上取好再交给撤销：它跑在后台线程里。
        let clients = self.clients_for(&involved, cx);
        if clients.is_empty() {
            self.undo_now(&clients, &involved, cx);
            return;
        }
        // 撤销涉及远程：搬回去要下载或上传，那是网络，不能堵在 UI 线程上。
        // 撤销栈整条搬进后台线程，跑完再原样搬回来——
        // 撤不动时要能把这条记录放回栈里重试，所以不能只搬栈顶一条。
        let history = std::mem::take(&mut self.history);
        self.enqueue(
            Job {
                label: "正在撤销…".to_string(),
                total: 1,
                can_pause: false,
                run: Box::new(move |_control, tx| {
                    let mut history = history;
                    let result = history.pop_undo(&clients);
                    let _ = tx.try_send(WorkerMessage::Done(JobResult::Undone {
                        history,
                        involved,
                        result,
                    }));
                }),
            },
            cx,
        );
    }

    /// 当场执行撤销（只涉及本地路径时）：一次 `rename` 的事，不必惊动后台。
    fn undo_now(
        &mut self,
        clients: &HashMap<String, Arc<Dav>>,
        involved: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        match self.history.pop_undo(clients) {
            Ok(describe) => {
                self.last_message = Some(format!("已撤销{describe}"));
                self.last_message_is_error = false;
                cx.notify();
                cx.emit(OpsEvent::Changed {
                    removed: Vec::new(),
                    changed: involved.to_vec(),
                    renamed_to: Vec::new(),
                });
            }
            Err(error) => {
                self.last_message = Some(format!("撤销失败：{error}"));
                self.last_message_is_error = true;
                cx.notify();
            }
        }
    }

    // ---- 远程 ----

    /// 取某个远程路径所属连接的客户端。本地路径或没有这条连接时是 `None`。
    fn client_for(&self, path: &Path, cx: &mut App) -> Option<Arc<Dav>> {
        let (id, _) = remote::parse_location(path)?;
        self.remotes
            .as_ref()?
            .update(cx, |remotes, cx| remotes.client(&id, cx))
    }

    /// 一次搬运两端的客户端；两端都在本地时两个都是 `None`。
    fn remote_sides(
        &self,
        sources: &[PathBuf],
        dest: &Path,
        cx: &mut App,
    ) -> RemoteSides {
        RemoteSides {
            source: sources.first().and_then(|path| self.client_for(path, cx)),
            dest: self.client_for(dest, cx),
        }
    }

    /// 撤销要用到的客户端：把涉及的远程路径按连接 id 收一遍。
    ///
    /// 撤销是后来才按的，那时候才去取客户端——客户端可能被删掉重建过
    /// （改密码会清掉旧的），早取一份存着反而会用上失效的那个。
    fn clients_for(&self, paths: &[PathBuf], cx: &mut App) -> HashMap<String, Arc<Dav>> {
        let mut clients = HashMap::new();
        for path in paths {
            let Some((id, _)) = remote::parse_location(path) else {
                continue;
            };
            if clients.contains_key(&id) {
                continue;
            }
            if let Some(client) = self.client_for(path, cx) {
                clients.insert(id, client);
            }
        }
        clients
    }

    // ---- 拷贝 / 移动 ----

    /// 默认“双方都保留”直接开始；有冲突先问。
    fn begin_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        cx: &mut Context<Self>,
    ) {
        // 目标是远程时 `exists()` 永远是假，判重得问服务器——
        // 而问服务器是网络，不能堵在 UI 线程上，所以那一趟也排进后台。
        let Some(client) = self.client_for(&dest, cx) else {
            let collisions = sources
                .iter()
                .filter(|source| {
                    source
                        .file_name()
                        .is_some_and(|name| dest.join(name).exists())
                })
                .count();
            self.after_preflight(sources, dest, cut, collisions, cx);
            return;
        };

        let checking = sources.clone();
        let into = dest.clone();
        self.enqueue(
            Job {
                label: "正在检查目标目录…".to_string(),
                total: sources.len(),
                can_pause: false,
                run: Box::new(move |_control, tx| {
                    let collisions = checking
                        .iter()
                        .filter(|source| {
                            source
                                .file_name()
                                .is_some_and(|name| ops::remote_exists(&client, &into.join(name)))
                        })
                        .count();
                    let _ = tx.try_send(WorkerMessage::Done(JobResult::Preflight {
                        sources: checking,
                        dest: into,
                        cut,
                        collisions,
                    }));
                }),
            },
            cx,
        );
    }

    /// 预检有了结论：有冲突先问，没有就直接开始。
    fn after_preflight(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        collisions: usize,
        cx: &mut Context<Self>,
    ) {
        if collisions > 0 {
            self.conflict = Some(Conflict {
                sources,
                dest,
                cut,
                collisions,
            });
            cx.notify();
            return;
        }

        self.start_transfer(sources, dest, cut, ConflictPolicy::KeepBoth, cx);
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        policy: ConflictPolicy,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let remote = self.remote_sides(&sources, &dest, cx);
        let total = ops::count_entries(&sources, remote.source.as_deref());
        let label = if cut {
            format!("正在移动 {} 项", sources.len())
        } else {
            format!("正在拷贝 {} 项", sources.len())
        };
        let retry_dest = dest.clone();

        self.enqueue(
            Job {
                label,
                total,
                can_pause: true,
                run: Box::new(move |control, tx| {
                    // 每来一次进度都先看一眼手柄：暂停就在这里等到恢复，
                    // 中止就返回 false，拷贝随即停在当前数据块的边界。
                    let mut report = |progress: &Progress| {
                        if !control.should_continue() {
                            return false;
                        }
                        tx.try_send(WorkerMessage::Progress(progress.clone()))
                            .is_ok()
                    };
                    let outcome = if cut {
                        ops::move_entries(&sources, &dest, policy, &remote, &mut report)
                    } else {
                        ops::copy_entries(&sources, &dest, policy, &remote, &mut report)
                    };
                    let aborted = control.state() == ControlState::Aborting;
                    let _ = tx.try_send(WorkerMessage::Done(JobResult::Transferred {
                        outcome,
                        cut,
                        aborted,
                        dest: retry_dest,
                        policy,
                    }));
                }),
            },
            cx,
        );
    }

    fn start_trash(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let total = paths.len();
        self.enqueue(
            Job {
                label: format!("正在移到回收站 {total} 项"),
                total,
                can_pause: false,
                run: Box::new(move |_control, tx| {
                    let trashed = ops::trash(&paths);
                    let _ = tx.try_send(WorkerMessage::Done(JobResult::Trashed(trashed)));
                }),
            },
            cx,
        );
    }

    /// 排一个后台任务。一次只跑一个，先来先跑。
    fn enqueue(&mut self, job: Job, cx: &mut Context<Self>) {
        self.queue.push_back(job);
        self.pump_queue(cx);
    }

    /// 没有任务在跑就把队首那个放出去。
    fn pump_queue(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let Some(job) = self.queue.pop_front() else {
            return;
        };
        self.start_job(job, cx);
    }

    /// 真的起一个后台任务：`job.run` 在普通线程里跑，
    /// 进度和结论经 channel 回到 UI。
    fn start_job(&mut self, job: Job, cx: &mut Context<Self>) {
        let Job {
            label,
            total,
            can_pause,
            run,
        } = job;

        let control = Control::new();
        self.control = Some(control.clone());
        self.running = Some(Running::new(label, total, can_pause));
        self.last_message = None;
        cx.notify();

        // 无界通道：进度是按数据块来的，一个 1 GB 的文件有一千条。
        // 用有界通道的话，UI 稍微慢一点 `try_send` 就会失败，
        // 而"发送失败"在回调里的含义是"用户要中止"——那会莫名其妙断掉拷贝。
        let (tx, rx) = async_channel::unbounded();

        // 真正干活的线程：同步 fs 操作 + 进度回调。
        std::thread::spawn(move || run(control, tx));

        // UI 侧消费：进度与结论。
        // async move 前缀形式 + 克隆 AsyncApp：future 持有所有权，跨 await 安全。
        cx.spawn(async move |this: WeakEntity<Ops>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            while let Ok(message) = rx.recv().await {
                this.update(&mut cx, |ops, cx| ops.on_worker_message(message, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn on_worker_message(&mut self, message: WorkerMessage, cx: &mut Context<Self>) {
        match message {
            WorkerMessage::Progress(progress) => {
                if let Some(running) = self.running.as_mut() {
                    running.done = progress.done.min(running.total);
                    running.label = progress.label;
                    running.bytes_done = progress.bytes_done;
                    running.bytes_total = progress.bytes_total;
                }
                cx.notify();
            }
            WorkerMessage::Done(result) => {
                self.finish_job(result, cx);
            }
        }
    }

    /// 收尾当前任务，然后把队列里的下一个放出去。
    fn finish_job(&mut self, result: JobResult, cx: &mut Context<Self>) {
        self.running = None;
        self.control = None;
        match result {
                    JobResult::Trashed(trashed) => {
                        if trashed.is_empty() {
                            self.last_message = Some("没有项目能移进回收站".to_string());
                            self.last_message_is_error = true;
                        } else {
                            // 删除会让更早的“新建/复制”记录失效，先清理再压栈。
                            for (path, _) in &trashed {
                                self.history.note_removed(path);
                            }
                            self.history.push(ops::HistoryEntry::Trashed {
                                originals: trashed.clone(),
                            });
                            let removed: Vec<PathBuf> = trashed
                                .iter()
                                .map(|(path, _)| path.clone())
                                .collect();
                            self.last_message = Some(format!(
                                "已移到回收站 {} 项",
                                trashed.len()
                            ));
                            self.last_message_is_error = false;
                            cx.emit(OpsEvent::Changed {
                                removed,
                                changed: Vec::new(),
                                renamed_to: Vec::new(),
                            });
                        }
                    }
                    JobResult::BulkRenamed { renamed, failed } => {
                        if !renamed.is_empty() {
                            // 先把更早的记录指向新路径，再压入整批记录。
                            for (from, to) in &renamed {
                                self.history.note_renamed(from, to);
                            }
                            self.history.push(ops::HistoryEntry::BulkRenamed {
                                moves: renamed.clone(),
                            });
                        }
                        let mut message = format!("已重命名 {} 项", renamed.len());
                        if !failed.is_empty() {
                            message.push_str(&format!(
                                "，{} 项失败（{}）",
                                failed.len(),
                                failed[0].1
                            ));
                        }
                        self.last_message = Some(message);
                        self.last_message_is_error = !failed.is_empty();
                        cx.emit(OpsEvent::Changed {
                            removed: Vec::new(),
                            changed: Vec::new(),
                            renamed_to: renamed,
                        });
                    }
                    JobResult::Transferred {
                        outcome,
                        cut,
                        aborted,
                        dest,
                        policy,
                    } => {
                        // 失败项留着让人重试；被中止的不算失败，重试列表里不该出现。
                        self.retry = if outcome.failed.is_empty() {
                            None
                        } else {
                            Some(Retry {
                                sources: outcome
                                    .failed
                                    .iter()
                                    .map(|(path, _)| path.clone())
                                    .collect(),
                                dest,
                                cut,
                                policy,
                            })
                        };

                        let removed = if cut {
                            outcome
                                .transfers
                                .iter()
                                .map(|(from, _)| from.clone())
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let renamed_to = if cut {
                            outcome.transfers.clone()
                        } else {
                            Vec::new()
                        };

                        if cut {
                            self.history.push(ops::HistoryEntry::Moved {
                                moves: outcome.transfers.clone(),
                            });
                        } else if !outcome.touched.is_empty() {
                            self.history.push(ops::HistoryEntry::Copied {
                                created: outcome.touched.clone(),
                            });
                        }

                        // 剪切成功后剪贴板里被搬走的项要清掉。
                        if cut {
                            self.clipboard.paths.retain(|path| {
                                !outcome
                                    .transfers
                                    .iter()
                                    .any(|(from, _)| from == path)
                            });
                            if self.clipboard.paths.is_empty() {
                                self.clipboard = Clipboard::default();
                            }
                            cx.emit(OpsEvent::ClipboardChanged);
                        }

                        self.last_message = Some(summarize(&outcome, cut, aborted));
                        self.last_message_is_error = !outcome.failed.is_empty();
                        cx.emit(OpsEvent::Changed {
                            removed,
                            changed: outcome.touched,
                            renamed_to,
                        });
                    }
                    JobResult::Preflight {
                        sources,
                        dest,
                        cut,
                        collisions,
                    } => self.after_preflight(sources, dest, cut, collisions, cx),
                    JobResult::Undone {
                        history,
                        involved,
                        result,
                    } => {
                        // 栈整条先还回来：撤不动时那条记录还在里面，可以再试一次。
                        self.history = history;
                        match result {
                            Ok(describe) => {
                                self.last_message = Some(format!("已撤销{describe}"));
                                self.last_message_is_error = false;
                                cx.emit(OpsEvent::Changed {
                                    removed: Vec::new(),
                                    changed: involved,
                                    renamed_to: Vec::new(),
                                });
                            }
                            Err(error) => {
                                self.last_message = Some(format!("撤销失败：{error}"));
                                self.last_message_is_error = true;
                            }
                        }
                    }
        }
        cx.notify();

        // 这个跑完了，队列里排着的下一个该上了。
        self.pump_queue(cx);
    }
}

fn summarize(outcome: &Outcome, cut: bool, aborted: bool) -> String {
    let verb = if cut { "移动" } else { "拷贝" };
    let mut text = if aborted {
        format!("已中止：{verb}完成 {} 项", outcome.done)
    } else {
        format!("{verb}完成 {} 项", outcome.done)
    };
    if outcome.skipped > 0 && !aborted {
        text.push_str(&format!("，跳过 {} 项", outcome.skipped));
    }
    if !outcome.failed.is_empty() {
        let first = &outcome.failed[0];
        text.push_str(&format!(
            "，{} 项失败（{}：{}）",
            outcome.failed.len(),
            display_name(&first.0),
            first.1
        ));
    }
    text
}

fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    // 不能用 `use super::*`：那一层 glob 了 `gpui_kit::*`，
    // 里面有个 `test` 会把 `#[test]` 属性挤掉（递归展开报错）。
    use super::Running;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn the_bar_follows_bytes_when_they_are_known() {
        let mut running = Running::new("拷贝".to_string(), 4, true);
        running.bytes_total = 100;
        running.bytes_done = 25;
        assert!((running.fraction() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn the_bar_falls_back_to_items_when_bytes_are_unknown() {
        // 删除、重命名这类没有字节量的操作只能看条目数。
        let mut running = Running::new("删除".to_string(), 4, false);
        running.done = 1;
        assert!((running.fraction() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn the_rate_waits_half_a_second_before_it_speaks() {
        let mut running = Running::new("拷贝".to_string(), 1, true);
        running.bytes_total = 10_000;
        running.bytes_done = 1_000;

        assert!(
            running.bytes_per_second().is_none(),
            "刚起步那一下除出来的数抖得没法看，先别报"
        );
        sleep(Duration::from_millis(450));
        assert!(running.bytes_per_second().expect("半秒之后该报速度") > 0);
    }

    #[test]
    fn paused_time_does_not_count_against_the_rate() {
        let mut running = Running::new("拷贝".to_string(), 1, true);
        running.bytes_total = 10_000;
        running.bytes_done = 1_000;
        sleep(Duration::from_millis(450));
        let before = running.bytes_per_second().expect("速度");

        running.mark_paused();
        sleep(Duration::from_millis(300));
        running.mark_resumed();

        let after = running.bytes_per_second().expect("恢复之后还得有速度");
        // 暂停那 300 毫秒被扣掉了：不扣的话速度会掉到六成。
        assert!(
            after * 10 > before * 6,
            "暂停前 {before} B/s，恢复后 {after} B/s"
        );
    }

    #[test]
    fn remaining_needs_both_a_rate_and_a_total() {
        let mut running = Running::new("拷贝".to_string(), 1, true);
        assert!(
            running.remaining().is_none(),
            "总字节数都不知道，算不出还剩多久"
        );

        running.bytes_total = 10_000;
        running.bytes_done = 500;
        sleep(Duration::from_millis(450));
        assert!(running.remaining().expect("该算出剩余时间") > Duration::ZERO);

        // 拷完了就没有剩余。
        running.bytes_done = 10_000;
        assert_eq!(running.remaining(), Some(Duration::ZERO));
    }
}
