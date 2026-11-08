# 架构：一个核心，多个界面

> 面向后续开发。读完这份应该能回答三个问题：**这段代码该写在哪一层**、**状态到底存在谁手上**、**加一个新东西要动哪些文件**。
>
> 配套：能力清单见 [`core-capabilities.md`](core-capabilities.md)，三平台文件系统约束见 [`cross-platform-filesystem.md`](cross-platform-filesystem.md)，工程铁律见 [`../AGENTS.md`](../AGENTS.md)。
>
> 候选方案：[搜索与预览工具集成参考](search-preview-integration.md)（[HTML 阅读版](search-preview-integration.html)）——fzf / rg / fd / bat 的库形态、分层与引入条件；尚未集成，不是功能排期。

## 一句话定位

WFiles = **一个跨平台核心**（`wfiles-core`）+ **可替换的界面**（当前是 GPUI Kit）。

参照 QQ 那类多端客户端的切法：业务状态与文件操作在核心里**只有一份**，界面是订阅者与投影。换 GUI 框架、加 TUI、将来做别的壳，核心一行不改。

这不是"把函数抽成库"那种解耦。区别在**状态归谁**：库是被调用的工具，内核是**权威状态的持有者**，界面问它要、听它通知。

## 一、两层与依赖方向

| 层 | crate / 目录 | 管什么 | 允许依赖 |
|---|---|---|---|
| 核心 | `crates/wfiles-core/` | 目录与条目模型、排序过滤、搬运/撤销/续传、远程协议、搜索比较差异、查询、特殊文件系统对象、i18n 纯函数、日志、单实例 | 标准库 + 与界面无关的 crate |
| 界面 | 根 crate `wfiles`，`src/` | 窗口、窗格、渲染、焦点与选中、拖放手势、快捷键、菜单、缩略图解码、内嵌终端绘制、`Settings` 的 GPUI 全局槽位 | `wfiles-core` + 界面框架 |

**方向只有一条：界面 → core。** core 里没有反向通道，也不存 `Entity` / `Context` 这类界面句柄。

这条方向有三道护栏，不是靠自觉：

1. **依赖表**：`crates/wfiles-core/Cargo.toml` 里没有 `gpui-kit` / `gpui`，编译期就断了。
2. **源码扫描**：`crates/wfiles-core/tests/boundary.rs` 扫 core 源码不许出现界面框架导入，也不许出现反向的 `crate::<界面模块>`；它自带"扫描器是活的"自检。
3. **路径兼容**：`src/main.rs` 用 `pub use wfiles_core::{...}` 把模块 re-export 回 `crate::xxx`，所以界面里几千处 `crate::ops::...` 一个字都没改——**新代码照旧写 `crate::ops`，别写成 `wfiles_core::ops`**（两套写法并存会让"这到底在哪一层"变模糊）。

## 二、核心内部的三类东西

混在一起看会觉得 core 很杂，其实只有三类：

| 类型 | 例子 | 有没有状态 |
|---|---|---|
| **纯函数 / 纯数据** | `format` `query` `palette` `bulk_rename` `diff` `path_input` `cli` | 无。输入进去，输出出来 |
| **执行器** | `ops` `resume` `ftp` `sftp` `s3` `archive` `search` `analyze` `compare` | 只在一次调用期间有局部状态，调完就散 |
| **状态机内核** | `kernel` `contract` | 有。**权威状态就住在这儿** |

只有第三类会被界面"订阅"。前两类是工具，界面直接用。

## 三、状态归谁（最容易搞错的一张表）

| 状态 | 唯一持有者 | 界面怎么拿 |
|---|---|---|
| 撤销栈（"可撤销"的命脉） | `kernel::Kernel` 里的一份 `ops::History` | `Kernel::history()` / `Core::can_undo()` / `KernelEvent::UndoChanged` |
| 当前目录的列表与筛选排序 | `contract::Core` 里的一份 `Listing` | `listing()` / `listing_mut()` / `KernelEvent::ListingChanged` |
| 复制链接的策略 | `kernel::Kernel.link_policy`（**契约层/新前端**的权威值）；GUI 另有一份在 `Settings`（它的搬运走 UI 线程，不经过 Kernel） | `Kernel::link_policy()` / `KernelEvent::LinkPolicyChanged`；GUI 读 `Settings::link_policy()` |
| 用户设置（语言、限速、视图、收藏…） | **界面层的 `Settings`**（JSON 落盘） | `Settings::read(cx)` / `Settings::peek(cx, ..)` |

最后一行是**已知取舍**，不是遗漏：`Settings` 要挂进 GPUI 的全局槽位，天然是界面概念。core 只接收"这一次调用需要的参数"（比如把 `CopyOptions` 传进 `ops::*_with_options`），不读任何全局配置。所以核心在任何宿主里行为一致，测试也不用装 `Global`。

**推论**：新前端（TUI）要自己决定设置存哪儿，然后把这些参数显式传给 core。它不需要、也拿不到 `Settings`。

## 四、契约：新前端的唯一入口

`wfiles_core::contract` 是一层刻意做窄的接口。新前端只认这几个类型：

| 类型 | 是什么 |
|---|---|
| `Command` | 前端发出的**意图**（进目录、过滤、排序、复制、移动、删除、改名、新建、撤销、设链接策略） |
| `Event` | 一次命令的同步回答（`Listing(Snapshot)` 或 `Failure { message }`） |
| `Snapshot` / `Row` | 当前目录的只读快照与一行数据，**只带数据不带界面句柄** |
| `Core` | 持有 `Listing` + `Kernel`，`apply(Command) -> Event`，`subscribe() -> Receiver<KernelEvent>` |

两条规矩：

- **命令只描述意图**，不许出现按键、焦点、布局、`Entity`、闭包。
- **状态变化一律走事件流**。同步回执只回答"这次命令的结果"，不是完整状态。前端拿到事件自己决定要不要重画。

`async-channel` 是单消费者，所以 `Kernel::subscribe()` 给**每个订阅者一条自己的 channel**，广播语义。GUI、TUI、未来第三个前端互不抢消息。

**契约不负责异步，但负责"看得见、停得下"。** `apply()` 是同步的，搬运那几秒会把调用线程占住；
谁怕卡界面谁自己挪到别的线程（NSH 客户端每类操作配一个 `xxxAsync`，内部 `std::thread` +
`async-channel` 回传；GUI 走自己的后台任务队列）。但搬运**期间**的状态由 core 负责：

- **进度走事件流**：`KernelEvent::Progress { label, done, total, bytes_done, bytes_total }`，
  每搬完一项、每写完一个数据块各一次。前端从 `subscribe()` 的接收端读，不需要轮询 core。
- **中止靠事先取走的手柄**：`Core::begin_transfer()` 在发 `Copy`/`Move` **之前**调，
  返回一个 `ops::Control` 副本（背后是 `Arc<AtomicU8>`）。搬运期间 `Core` 被 `&mut self`
  独占，中途想叫停的线程拿不到锁——所以手柄必须事先取走，之后任何线程 `abort()`/`pause()`
  都生效，不经过任何锁。**这也正是"中止"不需要 core 起线程的原因。**

命令全表见 [`core-capabilities.md` 8.1](core-capabilities.md)。

最小接入示例就是 `examples/wfiles-tui.rs`：只依赖标准库和 core，全程不认识任何界面框架。它刻意覆盖契约的**三种形状**——普通命令（回执就是新清单：`cd` / `cp` / `undo` / `mkdir`）、只读查询（`preview` / `volumes` / `undo_label` / `conflict_policy`，**不是 `Command`**）、流式命令（`search` 立刻返回，命中分批从事件流收，收到 `SearchDone` 才收工）。`scripts/smoke-tui.sh` 用管道喂命令真跑一遍，逐项验输出（6 条）。**这也是"新增 TUI 不用改核心"的验收用例。**

## 五、一次操作走的两条路（看清哪里共用、哪里分岔）

**GUI 粘贴**（`src/ops_entity.rs`）：

```
Pane 收到 Cmd+V
  → Ops::start_transfer            读 Settings 得到限速与链接策略
  → 后台线程                        ops::copy_entries_with_options(...)
  → Outcome{done, failed, touched, warnings}
  → JobResult::Transferred         回到主线程
  → summarize()                    拼成一句人话（含降级说明）
  → 状态栏 + 撤销栈 push + 发 OpsEvent::Changed
```

**TUI 复制**（`crates/wfiles-core/src/contract.rs` → `kernel.rs`）：

```
Command::Copy
  → Core::apply
  → Kernel::transfer              用 kernel 里那份 link_policy / conflict_policy
  → ops::transfer_entries_deciding ← 同一个函数（GUI 也是它）
  → Outcome
  → ops::summarize                "拷贝完成 5 项，跳过 2 项"（一句话归 core）
  → KernelEvent::{Status, OperationFinished, UndoChanged}
```

**分岔在"谁来调度"**：GUI 有自己的任务队列、进度条、暂停/中止、冲突对话框（这些是瞬时展示状态，按边界本就该留界面）；TUI 是同步的，一条命令做完就回。

**但"冲突策略"不是展示状态，是决定**：留哪一份、替不替换、跳不跳过，这件事
在三种界面上语义完全一样，所以权威值在内核（`Command::SetConflictPolicy` +
`KernelEvent::ConflictPolicyChanged`）。**问用户的方式**才归界面：GUI 是弹一次窗
定整批策略，NSH 是一个循环切换的按钮，TUI 可以先问再搬。

**粒度也是决定，不是展示**：`ops::Conflicts` 分 `Uniform(策略)` 与
`Ask(回调)`——前者整批一次定，后者每碰到一项问一次。后者会把搬运变成"要等"的
函数（`transfer_entries_deciding` 在冲突点阻塞），这是刻意的：人回答问题本来
就要时间。契约层的接法是 `Core::begin_conflicts()` 开口子 →
`KernelEvent::ConflictHit { path }` 问 → `Answers::answer(policy)` 答，
**一问一答、按到达顺序配对**。GUI 先数一遍碰撞、整批问一次（`ops_entity` 的
`begin_transfer` → `after_preflight` → `resolve_conflict`），走的是 `Uniform`。

**共用的是业务本身**：`ops::*`、`CopyOptions`、`LinkPolicy`、`ConflictPolicy`、`Outcome`、`History`、`summarize`、`search::Search`、`preview::load`、`volumes::{detected, capacity_of}`。两条路都不许自己写一份"复制"，也不许自己拼一份搬运结论、自己维护一份后缀→语言名的表、自己再扫一遍三平台的挂载点或自己 `statvfs` 算容量。
NSH 的磁盘面板直接调 `volumes::detected()` 而不是 `Core::volumes()`：后者只是它的转发，而 NSH 的 `CORE` 在搬运期间被工作线程独占，界面不该为一次只读查询去抢那把锁。契约那条路由 `tests/contract.rs` 第 23 条钉住，两边是同一份实现。
撤销标签同理：`Core::undo_label()` / GUI 的 `Ops::undo_label()` 都转发 `History::last_describe()`，六个 `HistoryEntry` 各说各的，没有第二份措辞（契约第 24 条、NSH 第 31–33 条钉住）。只读查询一族都不开 `Command` 变体——全表见 `docs/core-capabilities.md` §8.1。

**搜索是"结论不在回执里"的那一类**：`Command::Search` 立刻返回，命中分批发
`KernelEvent::SearchBatch`、收尾发 `SearchDone`。GUI 那边是 `search_entity`
自己拿 `search::spawn` 的把手，TUI 走事件流——两边共用同一个走目录的实现，
差别只在"谁攒结果"。

## 六、加东西该动哪里

| 我想加… | 改这里 | 别动 |
|---|---|---|
| 新前端（TUI / 换 GUI） | 新建一个 crate，只 `use wfiles_core::contract::*`；照 `examples/wfiles-tui.rs` 起步 | 不改 core 一行。真需要新能力时先加 `Command`，再加实现 |
| 一个新的文件操作 | `crates/wfiles-core/src/ops.rs`（+ 需要的话 `KernelCommand` / `Command` / `KernelEvent`） | 不在界面里直接 `fs::copy` |
| 一个新的特殊文件系统对象 | `crates/wfiles-core/src/entry_kind.rs`：加 `EntryKind` 变体 + `classify` 分支 + 单测 | 不在遍历/搬运里各写一份 `is_xxx()` |
| 一个新的远程协议 | `crates/wfiles-core/src/remote.rs`（实现 `Remote` trait）+ 新模块，并在 `remote` 的构造处登记 | 不改 `ops` 的搬运流程：它只认 trait |
| 一个新的用户设置项 | `src/settings.rs` 加字段（**必带 `#[serde(default)]`**，枚举存字符串 id）→ `src/app.rs` 的 `PaletteAction` + `run_palette_action` + 设置卡 | 不在 core 里读设置 |
| 一句新界面文案 | 中文原文当 key，加进 `crates/wfiles-core/src/i18n.rs` 的 `EN` 表；界面用 `tr(cx, "…")` | 不在界面里写死英文 |
| 一条新快捷键 | 先 `grep` `src/keymap.rs` 确认没被占，再加 `KeyBinding` 并确认有 `.on_action` 处理 | 只绑键不接 handler（静默失效） |
| 一个新的界面面板 | `src/` 下新建 `xxx_entity.rs`，纯逻辑另开模块 | 把面板状态塞进 core |

## 七、护栏与判据

改核心前后该跑的东西：

| 命令 | 挡什么 |
|---|---|
| `cargo test -p wfiles-core` | core 单测 + `tests/boundary.rs`（依赖方向）+ `tests/contract.rs`（新前端验收） |
| `cargo test -p wfiles --bin wfiles -- --skip cmd_backspace_trashes_the_selection_and_undo_puts_it_back` | GUI 全量集成测试（那条 skip 是 macOS Finder 自动化权限，与本项目代码无关） |
| `cargo clippy --workspace --all-targets` / `cargo fmt --check` | 静态检查 |
| `cargo test --release perf:: -- --nocapture` | 性能基线（**看数字必须 release**） |

**加判据的纪律**（见 `AGENTS.md`「测试纪律」与 `MEMORY.md` 的"挑错层"）：新写的判据要先让它在**故意改坏代码**时红一次。绿的判据分不清"验过了"和"没验到"。

三个现成的例子，加新判据时可以照抄形状：

- `tests/contract.rs::a_new_frontend_can_choose_how_links_are_copied`：默认副本是普通文件，切到"保留链接"后同一条链路副本必须是链接。摘掉接线就红。
- `ops::tests::a_broken_link_still_occupies_its_name`：**必须让断链占在候选名上**（`a 2.txt`），占在 `a.txt` 上旧实现也测不出差别。
- `i18n::tests::translation_scan_reaches_the_ui_layer`：哨兵断言，钉住"扫描器真的扫到了界面层"。

## 八、已知边界（别当缺陷修）

| 边界 | 为什么这样 |
|---|---|
| `Settings` 留在界面层，core 不读全局配置 | 它要 GPUI 全局槽位；core 只吃显式参数，换宿主行为一致 |
| GUI 的搬运不走 `Kernel` | 它需要任务队列、进度、暂停、冲突对话框——都是瞬时展示状态，按边界该留界面。共用的是 `ops::*` 与 `CopyOptions` |
| `selection`（键盘光标 vs 多选拖放）留界面 | 两端的语义本来就不同，下沉到 core 会两边都别扭 |
| 契约还没做"逐项问冲突" | 进度、中止、**冲突策略**已接（事件 + `begin_transfer()` 手柄 + `SetConflictPolicy`）。没接的是逐项交互：那要给 `transfer` 加"这里冲突了，等你回答"的回调，把它从同步函数变成要等待的函数。一整批一个策略够用之前不加 |
| 同一进程里 GUI 的 `Ops` 和每个 `Pane` 的 `Core` 各持一个 `Kernel` | 当前每个窗格一个 `Core`，`Ops` 另有一个。撤销栈的"唯一"是**每个实例内唯一**。要真正全局唯一，得把 `Kernel` 提到 `App` 层再分发——没做，也还没定值不值得（跨窗格的撤销语义本身就要先想清楚） |
| Windows / Linux 只做到"代码层同等优先" | 本机只有 macOS。Windows 分支做过**三元类型检查**（见 `cross-platform-filesystem.md` 6.1），运行时仍需真机/CI |

## 九、读代码的顺序

1. `crates/wfiles-core/src/lib.rs` —— 十行说明边界与方向，先看这个
2. `crates/wfiles-core/src/contract.rs` + `kernel.rs` —— 状态与事件在哪
3. `crates/wfiles-core/src/entry_kind.rs` —— 特殊文件系统对象的统一语义（三平台规则都是纯函数）
4. `crates/wfiles-core/src/ops.rs` —— 搬运/撤销/续传的真身
5. `src/app.rs` → `src/pane.rs` → `src/ops_entity.rs` —— 界面怎么订阅、怎么调度
6. `examples/wfiles-tui.rs` —— 一个文件看懂：新前端最少需要什么
