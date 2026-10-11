//! 文件操作的 UI 侧枢纽：剪贴板、撤销栈、进行中的任务与待处理的对话框。
//!
//! 窗格只负责发起请求（“我想删这几个”），真正动手、汇报进度、
//! 弹确认与冲突选择都在这里；完成后广播 [`OpsEvent`]，各窗格自己对照路径刷新。

use std::{
    path::PathBuf,
    time::SystemTime,
};

use gpui_kit::{AsyncApp, WeakEntity, *};

use crate::bulk_rename::{self, BulkRename, BulkRenamePreview};
use crate::ops::{self, ConflictPolicy, History, Outcome, Progress};

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
    Transferred { outcome: Outcome, cut: bool },
    /// 批量重命名的结果。
    BulkRenamed {
        renamed: Vec<(PathBuf, PathBuf)>,
        failed: Vec<(PathBuf, String)>,
    },
}

pub struct Ops {
    clipboard: Clipboard,
    history: History,
    running: Option<Running>,
    confirm: Option<Confirm>,
    conflict: Option<Conflict>,
    prompt: Option<Prompt>,
    bulk: Option<BulkState>,
    /// 最近一次操作的结论，显示在底部状态栏。
    last_message: Option<String>,
    /// 上一条消息是不是错误（决定显示颜色）。
    last_message_is_error: bool,
}

impl Ops {
    pub fn new() -> Self {
        Self {
            clipboard: Clipboard::default(),
            history: History::default(),
            running: None,
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
        self.spawn_job(
            format!("正在批量重命名 {count} 项"),
            count,
            cx,
            move |tx| {
                let (renamed, failed) = bulk_rename::apply(&bulk.previews);
                let _ = tx.try_send(WorkerMessage::Done(JobResult::BulkRenamed {
                    renamed,
                    failed,
                }));
            },
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
        match self.history.pop_undo() {
            Ok(describe) => {
                self.last_message = Some(format!("已撤销{describe}"));
                self.last_message_is_error = false;
                cx.notify();
                cx.emit(OpsEvent::Changed {
                    removed: Vec::new(),
                    changed: involved,
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

    // ---- 拷贝 / 移动 ----

    /// 默认“双方都保留”直接开始；有冲突先问。
    fn begin_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        cx: &mut Context<Self>,
    ) {
        let collisions = sources
            .iter()
            .filter(|source| {
                let name = source.file_name();
                name.is_some_and(|name| dest.join(name).exists())
            })
            .count();

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
        let total = ops::count_entries(&sources);
        let label = if cut {
            format!("正在移动 {} 项", sources.len())
        } else {
            format!("正在拷贝 {} 项", sources.len())
        };

        self.spawn_job(label, total, cx, move |tx| {
            let outcome = if cut {
                ops::move_entries(&sources, &dest, policy, &mut |progress| {
                    tx.try_send(WorkerMessage::Progress(progress.clone())).is_ok()
                })
            } else {
                ops::copy_entries(&sources, &dest, policy, &mut |progress| {
                    tx.try_send(WorkerMessage::Progress(progress.clone())).is_ok()
                })
            };
            let _ = tx.try_send(WorkerMessage::Done(JobResult::Transferred { outcome, cut }));
        });
    }

    fn start_trash(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let total = paths.len();
        self.spawn_job(format!("正在移到回收站 {total} 项"), total, cx, move |tx| {
            let trashed = ops::trash(&paths);
            let _ = tx.try_send(WorkerMessage::Done(JobResult::Trashed(trashed)));
        });
    }

    /// 起一个后台任务：`worker` 在普通线程里跑，进度和结论经 channel 回到 UI。
    fn spawn_job<F>(&mut self, label: String, total: usize, cx: &mut Context<Self>, worker: F)
    where
        F: FnOnce(async_channel::Sender<WorkerMessage>) + Send + 'static,
    {
        let (tx, rx) = async_channel::bounded(16);
        self.running = Some(Running {
            label,
            done: 0,
            total: total.max(1),
        });
        self.last_message = None;
        cx.notify();

        // 真正干活的线程：同步 fs 操作 + 进度回调。
        std::thread::spawn(move || worker(tx));

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
                }
                cx.notify();
            }
            WorkerMessage::Done(result) => {
                self.running = None;
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
                    JobResult::Transferred { outcome, cut } => {
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

                        self.last_message = Some(summarize(&outcome, cut));
                        self.last_message_is_error = !outcome.failed.is_empty();
                        cx.emit(OpsEvent::Changed {
                            removed,
                            changed: outcome.touched,
                            renamed_to,
                        });
                    }
                }
                cx.notify();
            }
        }
    }
}

fn summarize(outcome: &Outcome, cut: bool) -> String {
    let verb = if cut { "移动" } else { "拷贝" };
    let mut text = format!("{verb}完成 {} 项", outcome.done);
    if outcome.skipped > 0 {
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
