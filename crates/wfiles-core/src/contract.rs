//! 所有前端共用的**稳定调用契约**。
//!
//! # 形状
//!
//! `Core` 是一个前端进程里唯一的入口：浏览状态（当前目录 / 过滤 / 排序）
//! 与操作状态（撤销栈）都由它持有，后者委托给 [`crate::kernel::Kernel`]。
//! 命令进 → 同步回执 `Event` 出；同时所有状态变化经 `subscribe()` 的事件流
//! 广播给订阅者。简单前端只看回执就够，需要持续跟随状态的前端订阅事件流。
//!
//! # 契约规则（改动前先读）
//!
//! 1. 只传数据：`PathBuf` / `String` / 数值 / 枚举。不出现 `Entity`、`Context`、
//!    `Window`、回调闭包，也不出现任何界面框架的类型。
//! 2. `Command` 只描述**意图**，不描述按键、鼠标、焦点或布局。
//! 3. 失败文案用 [`crate::i18n::Msg::say_in`] 渲染：纯函数，不需要 GPUI 的 `cx`。
//!    内核只给结构化错误，最终怎么显示仍归各端。
//! 4. 三平台上签名完全一致，没有 `#[cfg(target_os)]`，也没有新依赖。
//! 5. **状态只有一份**：撤销栈在 `Kernel` 里，GUI 的 `Ops` 与这里的 `Core`
//!    都委托同一个类型，不再各自 new 一个 `History`。
//! 6. 进度条、对话框、焦点与选中仍归界面层——它们是瞬时展示状态，
//!    TUI 的键盘光标和 GUI 的多选拖放语义不同，不硬统一。
//!
//! # 适用边界
//!
//! 覆盖「浏览 + 本地搬运 + 撤销」。远程搬运、续传、限速、比较、差异仍由
//! `wfiles_core` 的公开模块直接提供，新前端同样不需要改 core；要不要把它们
//! 也收进 `Command` / 事件流，是后续取舍，不是这一层的默认承诺。

use std::path::{Path, PathBuf};

use async_channel::Receiver;

use crate::entry_kind::LinkPolicy;
use crate::format;
use crate::i18n::Locale;
/// 搜索命中的形状由内核定义、在这里再导出：前端只需要认识 `contract::*`。
pub use crate::kernel::SearchHit;
use crate::kernel::{Ack, Kernel, KernelCommand, KernelEvent};
use crate::model::{Listing, Sort, SortKey};
/// 预览的形状也由 core 定义、在这里再导出：前端只需要认识 `contract::*`。
/// 语言名（`Some("Rust")`）与十六进制的排法（`HexRow::format`）都是 core 的知识，
/// 界面不该各自再维护一份后缀表。
pub use crate::preview::{HexPreview, HexRow, Kind, Preview, TextPreview};
/// 卷与容量的形状同理：`capacity` 是 `Option`，"探不出来"是一个合法答案。
pub use crate::volumes::{Capacity, Volume};

/// 列表里的一行。只带展示需要的字段，不带界面句柄。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub size_text: String,
    pub modified_text: String,
}

/// 当前目录的只读快照。前端每次拿到它重画即可，不用自己维护筛选/排序状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub path: PathBuf,
    pub rows: Vec<Row>,
    /// 磁盘上的真实总数（过滤前）。"N / M" 里的 M 用它。
    pub total_count: usize,
    /// 过滤后看得见的数量。
    pub shown_count: usize,
    pub show_hidden: bool,
    pub query: String,
    pub sort: Sort,
}

/// 前端发出的意图。与输入设备、框架、平台都无关。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// 重读当前目录。
    Refresh,
    /// 进入一个目录（TUI 里是敲路径，GUI 里可能是双击或点面包屑）。
    Enter(PathBuf),
    /// 回到上级目录。
    Up,
    /// 是否显示隐藏项。
    SetHidden(bool),
    /// 重名了怎么办：保留两者 / 替换 / 跳过。
    ///
    /// 与 `SetLinkPolicy` 同一套路：语义归内核，**问用户的方式归前端**
    /// （GUI 在冲突时弹一次窗，然后把整批的策略发下来；TUI 可以先问再搬）。
    SetConflictPolicy(crate::ops::ConflictPolicy),
    /// 复制/移动链接时是跟随目标内容还是保留链接本身。
    /// 权威值存在内核里，新前端只需发这一条，不用自己维护一份偏好。
    SetLinkPolicy(LinkPolicy),
    /// 过滤词。
    SetQuery(String),
    /// 从 `root` 往下搜（语法见 `crate::query::Query`：通配、大小、时间、
    /// `content:` 都能写）。**立刻返回**，命中的分批发到事件流
    /// （`KernelEvent::{SearchBatch, SearchDone}`）。
    ///
    /// 为什么不做成"同步返回结果"：搜索要走整棵树、还可能要读文件内容，
    /// 让 `apply` 等它 = 界面卡死。再发一条就是叫停上一趟。
    Search { root: PathBuf, query: String },
    /// 叫停正在跑的那趟搜索。没在搜也接受（幂等）。
    CancelSearch,
    /// 按某一列排序；点同一列翻转方向，沿用 `Sort::toggled` 的规则。
    SetSort(SortKey),
    /// 复制进目标目录（重名自动留双方），完成后可撤销。
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// 移动进目标目录，完成后可撤销。
    Move {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// 删除。
    ///
    /// `permanent: false` = **尽量进回收站**（可移动介质上仍然会彻底删除，
    /// 因为那些卷上的回收站靠不住）；`true` = 直接彻底删除、不进回收站、
    /// 不可撤销。前端要把"不能撤销"这句话说在确认框里。
    Delete {
        paths: Vec<PathBuf>,
        permanent: bool,
    },
    /// 同一目录内改名。
    Rename { from: PathBuf, to: String },
    /// 新建文件夹。
    CreateFolder { parent: PathBuf, name: String },
    /// 按模板新建文件（`template` 是 `templates::FileTemplate::id`：
    /// `plain` / `markdown` / `rust` / `python` / `shell` / `json` / `yaml` / `html`）。
    CreateFile {
        parent: PathBuf,
        name: String,
        template: String,
    },
    /// 撤销上一步。
    Undo,
}

/// core 对一次命令的同步回答。持续变化请看 `subscribe()` 的事件流。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Listing(Snapshot),
    Failure { message: String },
}

/// 一个前端持有的唯一入口。新建 TUI 或换 GUI 框架时，只需要这个类型。
#[derive(Debug)]
pub struct Core {
    path: PathBuf,
    show_hidden: bool,
    query: String,
    sort: Sort,
    locale: Locale,
    listing: Listing,
    /// 操作侧状态（撤销栈）与事件流的唯一持有者。
    kernel: Kernel,
}

impl Core {
    /// 从某个目录起。构造时不读盘：前端第一次 `apply(Command::Refresh)` 才取数。
    pub fn new(start: impl Into<PathBuf>) -> Self {
        let path = start.into();
        Self {
            path: path.clone(),
            show_hidden: false,
            query: String::new(),
            sort: Sort::default(),
            locale: Locale::default(),
            listing: Listing::empty(path),
            kernel: Kernel::new(),
        }
    }

    /// 语言是前端的属性，由前端告诉 core；core 不读系统设置、不读 GPUI 全局槽位。
    pub fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self.kernel.set_locale(locale);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn locale(&self) -> Locale {
        self.locale
    }

    /// 订阅状态变化。**每个前端一条独立的接收端**，事件广播给所有订阅者。
    pub fn subscribe(&mut self) -> Receiver<KernelEvent> {
        self.kernel.subscribe()
    }

    /// 取一个搬运的中止/暂停手柄（`ops::Control` 的副本）。
    ///
    /// **必须在发 `Copy` / `Move` 之前调**：搬运期间 `apply` 独占 `&mut self`，
    /// 想中途叫停的那个线程拿不到锁——所以手柄得事先取走。
    /// 拿到之后 `abort()` / `pause()` 可以从任何线程调，不经过任何锁
    /// （背后是一个 `Arc<AtomicU8>`），这也是"中止"不需要 core 起线程的原因。
    ///
    /// 每调一次换一个新的手柄：中止位不可逆，复用会让下一趟一开始就自己停下。
    pub fn begin_transfer(&mut self) -> crate::ops::Control {
        self.kernel.begin_transfer()
    }

    /// 开口子：下一趟搬运遇到重名时**一项一项问**，不再整批一个策略。
    ///
    /// 同样**必须在发 `Copy` / `Move` 之前调**，理由同上：搬运期间 `apply`
    /// 被独占。之后每次问到一项重名，事件流里来一条
    /// [`crate::kernel::KernelEvent::ConflictHit`]，`apply` 就停在这一项上等着，
    /// 直到有人 `Answers::answer(policy)`——答案可以从任何线程给。
    ///
    /// 不调就是老行为：`SetConflictPolicy` 定的那个策略对整批生效。两种粒度都
    /// 是有意的：批量搬一百份时逐项问一百次并不友好。
    pub fn begin_conflicts(&mut self) -> crate::kernel::Answers {
        self.kernel.begin_conflicts()
    }

    /// 本机现在挂着哪些卷，各剩多少。**只读查询**，跟 [`Core::preview`] 同类。
    ///
    /// 三平台的落点、去重、排序全在 `volumes` 里（macOS 的 `/Volumes`、
    /// Windows 的盘符、Linux 的 `/media/<用户>`），新前端不该自己再写一份——
    /// 那是三个平台的目录约定，不是界面知识。
    ///
    /// 带三秒缓存（卷会热插拔，但没有跨平台通知），所以连续调几乎不花钱。
    /// 用户点了"刷新"想立刻看插上的盘，就调 `volumes::rescan_now()`。
    ///
    /// `capacity` 是 `Option`：探不出来就是 `None`，**不是 0**——
    /// 界面该据此决定显不显示那一行，别把"不知道"画成"满了"。
    pub fn volumes(&self) -> Vec<crate::volumes::Volume> {
        crate::volumes::detected().as_ref().clone()
    }

    /// 看一眼这个文件会预览成什么样。**只读查询**——所以它不是一条 `Command`：
    /// 命令会走"操作落地"那条路（进撤销栈、推 `OperationFinished`），
    /// 而看内容什么都没改。
    ///
    /// 同步做：一次最多读 256 KB（`preview::Budget` 默认值），文件小就更快。
    /// 想要别的预算就直接用 `crate::preview::load_with`——那是 core 的公开模块，
    /// 这里只是给"只认识契约"的前端一个默认口径。
    ///
    /// 图片 / PDF / 音视频只回 `Preview::Opaque`：真解码（缩略图）是界面层的事，
    /// 而且判类型**绝不 open 文件**（云占位会因此触发下载，见 `cross-platform-filesystem.md`）。
    pub fn preview(&self, path: &Path) -> Preview {
        crate::preview::load(path)
    }

    /// 当前目录清单。**整个前端只有这一份**：GUI 不再自己 new 一个 `Listing`。
    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    pub fn listing_mut(&mut self) -> &mut Listing {
        &mut self.listing
    }

    /// 外部数据到达时注入清单：文件监听刷新、远程列目录、或已有前端自己读盘。
    /// 注入和命令一样推 `ListingChanged`，所以 GUI 也是订阅者，没有谁特殊。
    pub fn set_listing(&mut self, listing: Listing) {
        // Core 的浏览字段要跟注入进来的清单一致，否则下一次 `Refresh`
        // 会拿旧的 query / sort / path 重读，界面上看着像「状态回跳」。
        self.path = listing.path().to_path_buf();
        self.show_hidden = listing.shows_hidden();
        self.query = listing.query().to_owned();
        self.sort = listing.sort();
        self.listing = listing;
        let event = self.snapshot();
        let _ = self.emit_listing(event);
    }

    /// 撤销栈现在是否可用（按钮可用性）。不暴露整个 `History`：
    /// 前端只该问"能不能撤"，不该自己改栈。
    pub fn can_undo(&self) -> bool {
        !self.kernel.history().is_empty()
    }

    /// **下一步会撤销什么**，一句能上屏的话（「新建“草稿”」「复制 2 项」）。
    /// 栈空就是 `None`。
    ///
    /// `can_undo()` 只回答"能不能"，撤销按钮光写「撤销」等于让用户闭眼按
    /// `Cmd+Z`——GUI 版很早就把这句描述放在按钮上（`Ops::undo_label`），
    /// 但新前端拿不到。这里补的是**同一个** `History::last_describe()`，
    /// 不是给契约新造一套措辞：六种 `HistoryEntry` 各说各的，前端不许自己拼。
    ///
    /// 返回 `Msg` 而不是 `String`：需要换语言的前端用 `say_in(locale)`，
    /// 用 `Display`（`to_string()`）拿到的恒是中文——那是"还没上屏的中间态"
    /// 的口径，跟 [`crate::volumes::Capacity::describe`] 一样。
    /// **只读查询**：不弹栈、不推事件，也不许开成 `Command` 变体。
    pub fn undo_label(&self) -> Option<crate::i18n::Msg> {
        self.kernel.history().last_describe()
    }

    /// 当前重名策略。前端画选择器读它——和 `can_undo()` 一样只读。
    ///
    /// 前端**不要自己另存一份**：策略的权威值在内核，而 `SetConflictPolicy`
    /// 同时会广播 `ConflictPolicyChanged`，两处各存一份必然对不上
    /// （比如前端重建之后）。
    pub fn conflict_policy(&self) -> crate::ops::ConflictPolicy {
        self.kernel.conflict_policy()
    }

    /// 处理一条命令，返回这次的同步回答；状态变化同时推给订阅者。
    pub fn apply(&mut self, command: Command) -> Event {
        match command {
            Command::Refresh => self.reload(),
            Command::Enter(path) => {
                self.path = path;
                self.reload()
            }
            Command::Up => {
                if let Some(parent) = self.path.parent() {
                    self.path = parent.to_path_buf();
                }
                self.reload()
            }
            Command::SetHidden(show) => {
                self.show_hidden = show;
                self.restate()
            }
            Command::SetLinkPolicy(policy) => {
                let ack = self.kernel.apply(KernelCommand::SetLinkPolicy(policy));
                self.after_operation(ack)
            }
            Command::SetConflictPolicy(policy) => {
                let ack = self.kernel.apply(KernelCommand::SetConflictPolicy(policy));
                self.after_operation(ack)
            }
            Command::SetQuery(query) => {
                self.query = query;
                self.restate()
            }
            Command::Search { root, query } => {
                // 搜索不改眼前这份清单：回执给的是**当前快照**（纯内存，不重读盘），
                // 命中走事件流。别在回执里塞结果——"看一眼状态"和"拿一次结果"
                // 会变成同一件事，而搜索可能要给几千条。
                let ack = self.kernel.apply(KernelCommand::Search { root, query });
                match ack {
                    Ack::Accepted => self.snapshot(),
                    Ack::Rejected { message } => Event::Failure { message },
                }
            }
            Command::CancelSearch => {
                let ack = self.kernel.apply(KernelCommand::CancelSearch);
                match ack {
                    Ack::Accepted => self.snapshot(),
                    Ack::Rejected { message } => Event::Failure { message },
                }
            }
            Command::SetSort(key) => {
                self.sort = self.sort.toggled(key);
                self.restate()
            }
            Command::Copy { sources, dest } => {
                let ack = self.kernel.apply(KernelCommand::Copy { sources, dest });
                self.after_operation(ack)
            }
            Command::Move { sources, dest } => {
                let ack = self.kernel.apply(KernelCommand::Move { sources, dest });
                self.after_operation(ack)
            }
            Command::Delete { paths, permanent } => {
                // `permanent: false` 也要问介质：可移动介质上没有可用的回收站。
                let mode = if permanent {
                    crate::ops::DeleteMode::Permanent
                } else {
                    crate::ops::delete_mode_for(&paths)
                };
                let ack = self.kernel.apply(KernelCommand::Delete { paths, mode });
                self.after_operation(ack)
            }
            Command::Rename { from, to } => {
                let ack = self.kernel.apply(KernelCommand::Rename { from, to });
                self.after_operation(ack)
            }
            Command::CreateFolder { parent, name } => {
                let ack = self
                    .kernel
                    .apply(KernelCommand::CreateFolder { parent, name });
                self.after_operation(ack)
            }
            Command::CreateFile {
                parent,
                name,
                template,
            } => {
                let ack = self.kernel.apply(KernelCommand::CreateFile {
                    parent,
                    name,
                    template,
                });
                self.after_operation(ack)
            }
            Command::Undo => {
                let ack = self.kernel.apply(KernelCommand::Undo);
                self.after_operation(ack)
            }
        }
    }

    /// 操作可能改了当前目录的内容，落地后重读并推 `ListingChanged`。
    fn after_operation(&mut self, ack: Ack) -> Event {
        let _ = self.reload();
        match ack {
            Ack::Accepted => self.snapshot(),
            Ack::Rejected { message } => Event::Failure { message },
        }
    }

    /// 只回答"现在是什么"，不重读盘。前端重画时可以反复调。
    pub fn snapshot(&self) -> Event {
        let listing = &self.listing;
        let mut rows = Vec::with_capacity(listing.visible_count());
        for position in 0..listing.visible_count() {
            let Some(entry) = listing.visible_entry(position) else {
                continue;
            };
            rows.push(Row {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                size: entry.size,
                size_text: if entry.is_dir {
                    String::from("—")
                } else {
                    format::file_size(entry.size)
                },
                modified_text: format::modified(entry.modified),
            });
        }
        Event::Listing(Snapshot {
            path: listing.path().to_path_buf(),
            total_count: listing.total_count(),
            shown_count: listing.visible_count(),
            show_hidden: listing.shows_hidden(),
            query: listing.query().to_owned(),
            sort: listing.sort(),
            rows,
        })
    }

    /// 状态变化优先走 `Listing` 的纯内存 refilter / resort，**不重读盘**。
    /// 这正是隐藏项开关与过滤框在十万项目录里也不卡的原因，别改成 reload。
    fn restate(&mut self) -> Event {
        let listing = &mut self.listing;
        listing.set_show_hidden(self.show_hidden);
        listing.set_query(&self.query);
        listing.set_sort(self.sort);
        self.emit_listing(self.snapshot())
    }

    fn reload(&mut self) -> Event {
        match Listing::read(&self.path, self.show_hidden) {
            Ok(mut listing) => {
                listing.set_query(&self.query);
                listing.set_sort(self.sort);
                self.listing = listing;
                self.emit_listing(self.snapshot())
            }
            Err(message) => {
                // 进不去就丢掉旧清单，别让前端继续画上一个目录的内容。
                self.listing = Listing::empty(self.path.clone());
                let text = message.say_in(self.locale);
                self.kernel.emit(KernelEvent::Status {
                    message: text.clone(),
                    is_error: true,
                });
                Event::Failure { message: text }
            }
        }
    }

    fn emit_listing(&mut self, event: Event) -> Event {
        if let Event::Listing(ref snapshot) = event {
            self.kernel.emit(KernelEvent::ListingChanged {
                path: snapshot.path.clone(),
                shown: snapshot.shown_count,
                total: snapshot.total_count,
            });
        }
        event
    }
}
