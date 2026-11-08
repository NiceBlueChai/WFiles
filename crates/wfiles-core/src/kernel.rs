//! `Kernel`：core 的操作侧状态机与事件流。
//!
//! # 这一层按什么形状设计
//!
//! 「同一核心、多界面」里，内核持有的是**权威的持久状态**并往外推事件；
//! 各前端是订阅者，自己决定怎么画。这里持的就是 WFiles 最核心的那份状态：
//! **撤销栈**（「可撤销」这个卖点的命脉）。一个前端进程内只有这一份实例，
//! GUI 的 `Ops` 与 TUI 的 `contract::Core` 都委托它，不再各自 new 一个 `History`。
//!
//! 故意**不**放进来的是瞬时展示状态：进度条、对话框、焦点与选中。它们的
//! 语义跟输入设备绑死（TUI 是键盘光标，GUI 是多选加拖放），下沉只会两边
//! 都别扭。这是 QQ 那类多端客户端的常见切法：共享持久状态与协议，各端
//! 自己写交互。
//!
//! # 依赖方向
//!
//! 只用标准库与 core 已有的 `async-channel`；没有新依赖、没有 `#[cfg(target_os)]`、
//! 不认识任何界面框架。文件操作全部复用 [`crate::ops`]，这一层不重写业务逻辑。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_channel::{Receiver, Sender};

use crate::entry_kind::{CopyOptions, LinkPolicy};
use crate::i18n::{Locale, Msg};
use crate::ops::{self, ConflictPolicy, DeleteMode, History, HistoryEntry, RemoteSides};
use crate::remote::Client;

/// 内核推给所有订阅者的事件。界面拿到后自己决定画什么，不回问内核。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelEvent {
    /// 目录快照变了（进目录、刷新、过滤、排序、隐藏项开关都算）。
    ListingChanged {
        path: PathBuf,
        shown: usize,
        total: usize,
    },
    /// 一次操作落地：这些路径出现了/消失了，前端按自己的目录决定要不要刷新。
    ///
    /// `skipped` 单独给：被跳过的东西**磁盘上什么都没变**，前端拿它去刷新目录
    /// 是白跑一趟；它只用来解释"为什么选中的每一项都报了成功，却只搬来一半"
    /// （重名策略选了「跳过」时的正常结果）。
    OperationFinished {
        touched: Vec<PathBuf>,
        failed: Vec<(PathBuf, String)>,
        skipped: usize,
    },
    /// 撤销栈变了：按钮可用性与「撤销 XXX」文案跟着走。
    UndoChanged { available: bool, label: String },
    /// 链接策略变了（前端的设置开关据此对齐，不自己另存一份）。
    LinkPolicyChanged { preserve: bool },
    /// 重名策略变了（前端的选择器据此对齐）。
    ConflictPolicyChanged { policy: ConflictPolicy },
    /// 搬运**问到一项重名**，等前端回话。
    ///
    /// 收到它说明这趟搬运正停在这一项上（[`Kernel::begin_conflicts`] 开过口子），
    /// 前端要问用户，然后把答案交给 [`Answers::answer`]。不问就一直等着。
    /// `path` 是已经拼好的落点（`dest/名字`）——用户要判断的是"要不要盖掉那一个"。
    ///
    /// 一问一答、按到达顺序配对：前端只能回答刚收到的那一条，不许提前攒答案。
    ConflictHit { path: PathBuf },
    /// 搬运进度：每搬完一项、以及每写完一个数据块各一次。
    ///
    /// 这是唯一一种**高频**事件。前端可以不看（不知道就不会卡），
    /// 要看就按 `bytes_done / bytes_total` 画进度条、按两次采样算速度。
    /// `bytes_total == 0` 表示数不出来（没权限），这时只能按条目数显示。
    Progress {
        label: String,
        done: usize,
        total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// 搜索命中了一批。**分批而不是逐条**：几万条逐条跨线程发能把队列淹了。
    ///
    /// 前端自己攒这批结果；core 不留结果列表——搜索结果属于"这次会话的
    /// 瞬时展示状态"，跟选中项、滚动位置同类。
    SearchBatch { hits: Vec<SearchHit> },
    /// 一趟搜索走完了（走完、撞到条数上限、或被叫停都算）。
    SearchDone {
        visited: usize,
        matched: usize,
        truncated: bool,
        cancelled: bool,
    },
    /// 状态栏那句话（已按当前语言渲染好）。
    Status { message: String, is_error: bool },
}

/// 一条搜索命中，只带展示需要的字段。
///
/// `search::Hit` 里有 `SystemTime` 和内部表示，没法跟 `KernelEvent` 一起
/// 比较相等；这一层是给事件的**稳定形状**，界面拿它画行。大小/时间的文案
/// 由前端用 [`crate::format`] 自己渲染（跟列表行同一套路）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 内容命中的那一行：行号（从 1 起）+ 原文。只在查询里有 `content:` 时才有。
    pub line: Option<(usize, String)>,
}

impl From<crate::search::Hit> for SearchHit {
    fn from(hit: crate::search::Hit) -> Self {
        Self {
            path: hit.path,
            name: hit.name,
            is_dir: hit.is_dir,
            size: hit.size,
            line: hit.line.map(|line| (line.number, line.text)),
        }
    }
}

/// 往每个订阅者那儿塞一条事件。关掉的那条直接丢弃，别攒在没人读的 channel 里。
///
/// 单独提出来是因为搬运与搜索都在**别的线程**上推送（拿不到 `&mut self`），
/// 只能先克隆一组 `Sender` 再往这儿喂。
fn broadcast(senders: &[Sender<KernelEvent>], event: KernelEvent) {
    for sender in senders {
        let _ = sender.try_send(event.clone());
    }
}

/// 一次命令的即时回执。**状态变化一律走事件流**，别在回执里塞完整状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    Accepted,
    Rejected { message: String },
}

/// 逐项问重名时「回答」的那一头。
///
/// 与 [`ops::Control`] 同一套路，理由也一样：搬运期间 `Kernel` 被调用线程独占，
/// 想在中途说句话的人拿不到锁。所以答案走一条**不经过任何锁**的通道：
/// 前端拿这个句柄，搬运动的那条线程在通道另一头等着。
///
/// 能克隆，因为界面线程答话时通常已经在别处了。丢掉全部句柄不会把搬运挂死——
/// 内核那一头会退回整批策略（见 [`Kernel::transfer`]）。
#[derive(Clone, Debug)]
pub struct Answers {
    sender: Sender<ConflictPolicy>,
}

impl Answers {
    /// 回答刚问到的那一项。**一次问、一次答**：多答的那些会和后面的问题错位。
    pub fn answer(&self, policy: ConflictPolicy) {
        // 无界通道，装不下只可能是内核那一头已经收工了，那就没人在等。
        let _ = self.sender.try_send(policy);
    }
}

/// 操作侧命令：只描述意图，不含按键、焦点、布局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelCommand {
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// 删除。`mode` 由调用方算好（可移动介质上是 `Permanent`）：
    /// 内核只负责执行和记账，不猜用户想要哪种删除。
    Delete {
        paths: Vec<PathBuf>,
        mode: DeleteMode,
    },
    /// 同一目录内改名。
    Rename {
        from: PathBuf,
        to: String,
    },
    /// 新建文件夹。
    CreateFolder {
        parent: PathBuf,
        name: String,
    },
    /// 按模板新建文件（`template` 是 `templates::FileTemplate::id`）。
    CreateFile {
        parent: PathBuf,
        name: String,
        template: String,
    },
    Undo,
    /// 复制时是跟随链接还是保留链接。核心里只有这一份，前端不各存一份。
    SetLinkPolicy(LinkPolicy),
    /// 重名了怎么办（保留两者 / 替换 / 跳过）。同样是核心里的一份，
    /// 前端负责问用户，再发这一条——问的方式归界面，语义归这里。
    SetConflictPolicy(ConflictPolicy),
    /// 从 `root` 往下搜，查询语法见 [`crate::query::Query`]。
    ///
    /// **立刻返回**，结果走事件流（`SearchBatch` / `SearchDone`）：搜索要走整棵
    /// 树、还可能要读文件内容，同步等它等于界面卡死。再发一条就是在叫停上一趟
    /// （旧把手一被丢掉就作废）。
    Search {
        root: PathBuf,
        query: String,
    },
    /// 叫停正在跑的那趟搜索。没在搜也接受——取消是幂等的。
    CancelSearch,
}

/// 一次操作的结果：落地的路径，加上因为目标已存在而被跳过的项数。
///
/// 只有搬运会产出 `skipped`；新建、改名、删除、撤销都是"要么成要么不成"，
/// 用 `From<Vec<PathBuf>>` 直接填 0，别为了统一类型让每个分支都写一遍。
struct Applied {
    touched: Vec<PathBuf>,
    skipped: usize,
}

impl From<Vec<PathBuf>> for Applied {
    fn from(touched: Vec<PathBuf>) -> Self {
        Self {
            touched,
            skipped: 0,
        }
    }
}

/// 唯一的状态持有者。
#[derive(Debug)]
pub struct Kernel {
    history: History,
    locale: Locale,
    link_policy: LinkPolicy,
    /// 重名时怎么办。跟链接策略一样是**内核里的权威值**：
    /// 前端负责问用户，问完发 `SetConflictPolicy` 下来，搬运时按这一份走。
    conflict_policy: ConflictPolicy,
    /// 正在跑的那趟搬运的中止/暂停手柄。**每趟换一个新的**：
    /// 中止位一旦置上就不可逆，复用会让第二次搬运一开始就自己停下。
    control: ops::Control,
    /// 正在跑的那趟搜索的把手。**丢掉即叫停**（`Cancel` 的 `Drop` 就干这个），
    /// 所以"换一趟新的"和"取消"都是同一件事：把这个 `Option` 换掉。
    search_cancel: Option<crate::search::Cancel>,
    /// 逐项问重名的应答通道。`Some` = 下一趟搬运会一项一项问。
    ///
    /// 跟 `control` 一样是**每趟换新的**：`transfer` 把它取走（`take`），
    /// 一趟问完这段对话就结束了，下一趟要么重新开口子、要么整批一个策略。
    conflict_answers: Option<Receiver<ConflictPolicy>>,
    /// 每个订阅者一条自己的 channel：async-channel 是单消费者，
    /// 用一组 Sender 才能把同一事件广播给 GUI / TUI / 未来的新端。
    subscribers: Vec<Sender<KernelEvent>>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            history: History::default(),
            locale: Locale::default(),
            link_policy: LinkPolicy::default(),
            conflict_policy: ConflictPolicy::default(),
            search_cancel: None,
            conflict_answers: None,
            control: ops::Control::new(),
            subscribers: Vec::new(),
        }
    }

    /// 换一个全新的中止/暂停手柄，返回它的副本。
    ///
    /// **必须在发搬运命令之前调用**：传输期间 `Kernel` 被调用线程独占
    /// （`&mut self`），此时再想拿手柄是拿不到的。拿到副本后，
    /// 随便哪个线程 `abort()` / `pause()` 都生效——句柄背后是一个
    /// `Arc<AtomicU8>`，不经过任何锁。
    pub fn begin_transfer(&mut self) -> ops::Control {
        self.control = ops::Control::new();
        self.control.clone()
    }

    pub fn link_policy(&self) -> LinkPolicy {
        self.link_policy
    }

    /// 开口子：「下一趟搬运遇到重名，一项一项问我」。
    ///
    /// **必须在发 `Copy` / `Move` 之前调**，跟 [`Kernel::begin_transfer`] 同一个理由：
    /// 搬运期间 `Kernel` 被 `&mut self` 独占，中途挂不上这条通道。拿到的句柄
    /// 可以克隆、可以交给别的线程——答案不经过任何锁。
    ///
    /// 不调就是老行为：整批一个策略（[`Kernel::conflict_policy`]），一次都不问。
    /// 两种粒度都留着是有意的——批量搬一百份时，逐项问一百次并不友好。
    ///
    /// 每趟都要重新调：`transfer` 会把通道取走，一趟问完这段对话就结束了。
    pub fn begin_conflicts(&mut self) -> Answers {
        let (sender, receiver) = async_channel::unbounded();
        self.conflict_answers = Some(receiver);
        Answers { sender }
    }

    /// 当前的重名策略。前端画选择器时读它，别自己另存一份。
    pub fn conflict_policy(&self) -> ConflictPolicy {
        self.conflict_policy
    }

    /// 换重名策略并广播。**语义归内核，问用户的方式归前端**：
    /// GUI 在冲突时弹一次窗、把整批的策略发下来；TUI 可以先问再搬。
    pub fn set_conflict_policy(&mut self, policy: ConflictPolicy) {
        self.conflict_policy = policy;
        self.emit(KernelEvent::ConflictPolicyChanged { policy });
    }

    /// 起一趟后台搜索，**立刻返回**：命中走事件流（`SearchBatch` / `SearchDone`）。
    ///
    /// 再调一次就是"换一趟"：旧把手被丢掉，旧搜索自己会尽快收手
    /// （[`crate::search::Cancel`] 的 `Drop` 干的就是这件事）。
    pub fn start_search(&mut self, root: &Path, text: &str) -> Result<(), Msg> {
        let query = crate::query::Query::parse(text)?;
        if query.is_empty() {
            // 空查询会命中所有东西，那不是搜索，是把整棵树搬上屏。
            return Err(crate::i18n::Message::msg("请先输入搜索词"));
        }
        // 先把订阅端克隆出来：搜索线程拿不到 `&mut self`。
        let senders = self.subscribers.clone();
        let sink: std::sync::Arc<dyn Fn(crate::search::Event) + Send + Sync> =
            std::sync::Arc::new(move |event| {
                let event = match event {
                    crate::search::Event::Batch(hits) => KernelEvent::SearchBatch {
                        hits: hits.into_iter().map(SearchHit::from).collect(),
                    },
                    crate::search::Event::Done(stats) => KernelEvent::SearchDone {
                        visited: stats.visited,
                        matched: stats.matched,
                        truncated: stats.truncated,
                        cancelled: stats.cancelled,
                    },
                };
                broadcast(&senders, event);
            });
        let search = crate::search::Search::new(root.to_path_buf(), query);
        // 赋值即换把手：旧的 `Cancel` 在这里被丢掉，旧搜索随之收手。
        self.search_cancel = Some(crate::search::spawn(search, sink));
        Ok(())
    }

    /// 叫停正在跑的那趟搜索。没在搜的时候调也安全（幂等）。
    pub fn cancel_search(&mut self) {
        // 丢掉把手就是叫停：`Cancel` 的 `Drop` 会置中止位。
        self.search_cancel = None;
    }

    /// GUI 的设置项也直接调这个：持久化在界面层，权威值只有内核这一份。
    pub fn set_link_policy(&mut self, policy: LinkPolicy) {
        self.link_policy = policy;
        self.emit(KernelEvent::LinkPolicyChanged {
            preserve: policy == LinkPolicy::Preserve,
        });
    }

    pub fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
        self.emit_undo_changed();
    }

    /// 订阅事件流。返回的是**只属于这个订阅者**的接收端。
    pub fn subscribe(&mut self) -> Receiver<KernelEvent> {
        let (tx, rx) = async_channel::unbounded();
        self.subscribers.push(tx);
        rx
    }

    /// GUI 的老路径要直接碰撤销栈（改名/删除后修正栈里路径），
    /// 但实例只有这一份，不复制。
    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }

    /// 撤销要跑在后台线程里，`History` 得整个搬走再搬回来。
    pub fn take_history(&mut self) -> History {
        std::mem::take(&mut self.history)
    }

    pub fn put_history(&mut self, history: History) {
        self.history = history;
        self.emit_undo_changed();
    }

    /// 命令的统一入口。真实的文件操作复用 `ops::*`，这里只负责
    /// 维护撤销栈并把变化推给订阅者。
    pub fn apply(&mut self, command: KernelCommand) -> Ack {
        let result = match command {
            // 改偏好不是文件操作，别复用下面"操作落地"的事件形状。
            KernelCommand::SetLinkPolicy(policy) => {
                self.set_link_policy(policy);
                return Ack::Accepted;
            }
            KernelCommand::SetConflictPolicy(policy) => {
                self.set_conflict_policy(policy);
                return Ack::Accepted;
            }
            KernelCommand::Search { root, query } => {
                // 搜索不算"操作落地"：它不改磁盘，也不进撤销栈。
                // 失败了要像别的命令一样回一句能读的话。
                return match self.start_search(&root, &query) {
                    Ok(()) => Ack::Accepted,
                    Err(message) => self.reject(message),
                };
            }
            KernelCommand::CancelSearch => {
                self.cancel_search();
                return Ack::Accepted;
            }
            KernelCommand::Copy { sources, dest } => self.transfer(&sources, &dest, false),
            KernelCommand::Move { sources, dest } => self.transfer(&sources, &dest, true),
            KernelCommand::Delete { paths, mode } => self.delete(&paths, mode).map(Applied::from),
            KernelCommand::Rename { from, to } => self.rename(&from, &to).map(Applied::from),
            KernelCommand::CreateFolder { parent, name } => {
                self.create_folder(&parent, &name).map(Applied::from)
            }
            KernelCommand::CreateFile {
                parent,
                name,
                template,
            } => self
                .create_file(&parent, &name, &template)
                .map(Applied::from),
            KernelCommand::Undo => self.undo().map(Applied::from),
        };
        match result {
            Ok(applied) => {
                self.emit(KernelEvent::OperationFinished {
                    touched: applied.touched,
                    failed: Vec::new(),
                    skipped: applied.skipped,
                });
                self.emit_undo_changed();
                Ack::Accepted
            }
            Err(message) => self.reject(message),
        }
    }

    /// 失败统一出口：一句能读的话既是回执，也推给订阅者（无界面的前端只看事件流）。
    fn reject(&mut self, message: Msg) -> Ack {
        let text = message.say_in(self.locale);
        self.emit(KernelEvent::Status {
            message: text.clone(),
            is_error: true,
        });
        Ack::Rejected { message: text }
    }

    fn transfer(&mut self, sources: &[PathBuf], dest: &Path, cut: bool) -> Result<Applied, Msg> {
        let options = CopyOptions {
            link_policy: self.link_policy,
            ..CopyOptions::default()
        };
        // 进度在闭包里广播：闭包不能碰 `self`（它正被这次搬运借走），
        // 所以先把接收端和手柄**克隆**出来。`Sender` 克隆的还是同一条 channel，
        // `Control` 克隆的还是同一个 `Arc<AtomicU8>`，都不影响语义。
        let senders = self.subscribers.clone();
        let control = self.control.clone();
        let mut sink = move |progress: &ops::Progress| {
            broadcast(
                &senders,
                KernelEvent::Progress {
                    label: progress.label.clone(),
                    done: progress.done,
                    total: progress.total,
                    bytes_done: progress.bytes_done,
                    bytes_total: progress.bytes_total,
                },
            );
            // 这一句同时管暂停（阻塞在这里）和中止（返回 false 让上层收手）。
            control.should_continue()
        };
        // 逐项问重名的通道是**这一趟专用**的：取走之后本次对话就结束了，
        // 下一趟要么重新开口子、要么回到整批策略。跟 `control` 一个道理。
        let answers = self.conflict_answers.take();
        let asked = self.subscribers.clone();
        let waiting = self.control.clone();
        let fallback = self.conflict_policy;
        // 问到一项就广播一条 `ConflictHit`，然后**停在这儿等人回话**。
        // 等待期间仍然盯着中止位：不然用户按了中止还得先把问题答完才停。
        let mut asker = |target: &Path| -> Option<ConflictPolicy> {
            let Some(queue) = answers.as_ref() else {
                return Some(fallback);
            };
            broadcast(
                &asked,
                KernelEvent::ConflictHit {
                    path: target.to_path_buf(),
                },
            );
            loop {
                if waiting.state() == ops::ControlState::Aborting {
                    return None;
                }
                match queue.try_recv() {
                    Ok(policy) => return Some(policy),
                    Err(async_channel::TryRecvError::Empty) => {
                        // 没有唤醒机制可用（`Core::apply` 在别的线程上阻塞着），
                        // 只能小步轮询。20ms 对人眼是不可感的，对 CPU 是无所谓的。
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    // 应答口全丢了：前端已经不要这趟了。退回整批策略把活干完，
                    // 别为了一个没人回答的问题把线程永远挂在这儿。
                    Err(async_channel::TryRecvError::Closed) => return Some(fallback),
                }
            }
        };
        let conflicts = if answers.is_some() {
            ops::Conflicts::Ask(&mut asker)
        } else {
            ops::Conflicts::Uniform(self.conflict_policy)
        };
        let mut outcome = if cut {
            ops::transfer_entries_deciding(
                sources,
                dest,
                conflicts,
                true,
                &RemoteSides::default(),
                &mut sink,
                options,
            )
        } else {
            ops::transfer_entries_deciding(
                sources,
                dest,
                conflicts,
                false,
                &RemoteSides::default(),
                &mut sink,
                options,
            )
        };
        // 跨卷搬运的元数据风险（xattr / ADS / 权限）。抽 20 项扫一次，
        // 只在跨卷时说话——同卷复制不会丢，每次都报就是噪音。
        if let Some(note) = crate::entry_kind::metadata_risk_note(sources, dest) {
            outcome.warnings.push(note);
        }
        // 结论那句话交给 core 拼（[`ops::summarize`]）：GUI 的状态栏与新前端
        // 必须说同一句话——"拷贝完成 5 项，跳过 2 项"这种句子不是界面知识。
        // 跳过项与降级说明都只在句子里当子句出现：躺在 `Outcome` 里没人看，
        // 等于静默改了用户选的语义。
        //
        // 用户叫停也走这一句（"已中止：拷贝完成 N 项"），别让前端把
        // "搬运只到一半"当成悄悄结束。
        let aborted = self.control.state() == ops::ControlState::Aborting;
        self.emit(KernelEvent::Status {
            message: ops::summarize(&outcome, cut, aborted).say_in(self.locale),
            is_error: !outcome.failed.is_empty(),
        });
        if outcome.done > 0 {
            let entry = if cut {
                HistoryEntry::Moved {
                    moves: std::mem::take(&mut outcome.transfers),
                }
            } else {
                HistoryEntry::Copied {
                    created: outcome.touched.clone(),
                }
            };
            self.history.push(entry);
        }
        if let Some((_, failure)) = outcome.failed.first() {
            let failed = outcome
                .failed
                .iter()
                .map(|(path, msg)| (path.clone(), msg.say_in(self.locale)))
                .collect::<Vec<_>>();
            self.emit(KernelEvent::OperationFinished {
                touched: outcome.touched.clone(),
                failed,
                skipped: outcome.skipped,
            });
            return Err(failure.clone());
        }
        Ok(Applied {
            touched: outcome.touched,
            skipped: outcome.skipped,
        })
    }

    fn delete(&mut self, paths: &[PathBuf], mode: DeleteMode) -> Result<Vec<PathBuf>, Msg> {
        let outcome = ops::delete(paths, mode, &HashMap::new());
        if outcome.deleted.is_empty() {
            return match outcome.failed.first() {
                Some((_, reason)) => Err(reason.clone()),
                None => Err(crate::i18n::Message::msg("没有项目能删除")),
            };
        }
        // 只有回收站能撤销；彻底删除进栈会让 Cmd+Z 去找一条不存在的记录。
        if !outcome.trashed_at.is_empty() {
            self.history.push(HistoryEntry::Trashed {
                originals: outcome.trashed_at.clone(),
            });
        }
        for (path, reason) in &outcome.failed {
            // 失败原因本身就是一句完整的话，别再套一层"删不掉：…"。
            self.emit(KernelEvent::Status {
                message: reason.say_in(self.locale),
                is_error: true,
            });
            crate::logfile::error(&format!("删不掉 {}：{reason}", path.display()));
        }
        Ok(outcome.deleted)
    }

    fn rename(&mut self, from: &Path, to: &str) -> Result<Vec<PathBuf>, Msg> {
        let renamed = ops::rename(from, to)?;
        // 先把更早的记录指向新路径，再压入本次记录，避免改到自己头上。
        self.history.note_renamed(from, &renamed);
        self.history.push(HistoryEntry::Renamed {
            from: from.to_path_buf(),
            to: renamed.clone(),
        });
        Ok(vec![renamed])
    }

    fn create_folder(&mut self, parent: &Path, name: &str) -> Result<Vec<PathBuf>, Msg> {
        let path = ops::create_folder_named(parent, name)?;
        self.history
            .push(HistoryEntry::Created { path: path.clone() });
        Ok(vec![path])
    }

    fn create_file(
        &mut self,
        parent: &Path,
        name: &str,
        template_id: &str,
    ) -> Result<Vec<PathBuf>, Msg> {
        let template = crate::templates::template(template_id).ok_or_else(|| {
            crate::i18n::Message::args("不认识的模板：{id}", &[("id", template_id.to_string())])
        })?;
        let path = ops::create_file_from_template(parent, name, template)?;
        self.history
            .push(HistoryEntry::Created { path: path.clone() });
        Ok(vec![path])
    }

    fn undo(&mut self) -> Result<Vec<PathBuf>, Msg> {
        let involved = self.history.last_paths();
        // 本地撤销不需要远程客户端；远程那一路仍由 remotes 层提供。
        let clients: HashMap<String, Client> = HashMap::new();
        let described = self.history.pop_undo(&clients)?;
        // `caused` 填的是 `{error}`，而这个 key 的占位符叫 `{what}`：
        // 用 args 装配，别让状态栏出现一句没填的「已撤销{what}」。
        let message =
            crate::i18n::Message::args("已撤销{what}", &[("what", described.say_in(self.locale))]);
        self.emit(KernelEvent::Status {
            message: message.say_in(self.locale),
            is_error: false,
        });
        Ok(involved)
    }

    fn emit_undo_changed(&mut self) {
        let label = self
            .history
            .last_describe()
            .map(|msg| msg.say_in(self.locale))
            .unwrap_or_default();
        self.emit(KernelEvent::UndoChanged {
            available: !self.history.is_empty(),
            label,
        });
    }

    /// 由 `contract::Core` 在命令落地后调用，把浏览侧的变化一并推给订阅者。
    /// 内核自己只推操作侧事件；两者合起来就是一条完整的事件流。
    pub fn emit(&mut self, event: KernelEvent) {
        // 关掉的订阅者顺手清掉，别让事件堆在一条没人读的 channel 里。
        self.subscribers
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("临时目录")
    }

    /// 彻底删除**不能留下"可撤销"**。
    ///
    /// 留了的话用户按 Cmd+Z 会去回收站找一条根本不存在的记录——
    /// 报一个"回收站里找不到"比没有撤销更让人困惑。
    #[test]
    fn a_permanent_delete_never_leaves_an_undo_entry() {
        let root = temp();
        let file = root.path().join("gone.txt");
        fs::write(&file, "x").expect("写文件");

        let mut kernel = Kernel::new();
        let events = kernel.subscribe();
        assert_eq!(
            kernel.apply(KernelCommand::Delete {
                paths: vec![file.clone()],
                mode: DeleteMode::Permanent,
            }),
            Ack::Accepted
        );
        assert!(!file.exists(), "文件该被彻底删掉");
        assert!(kernel.history().is_empty(), "彻底删除不该进撤销栈");

        let mut saw_undo_available = false;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event,
                KernelEvent::UndoChanged {
                    available: true,
                    ..
                }
            ) {
                saw_undo_available = true;
            }
        }
        assert!(!saw_undo_available, "彻底删除不该广播「可撤销」");
    }
}
