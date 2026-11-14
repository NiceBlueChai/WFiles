# 核心能力清单

> 后续开发先查这张表，别重新发明。每个域给出：**谁负责**、**入口在哪**、**动手前要知道什么**。
>
> 架构与分层见 [`architecture.md`](architecture.md)。三平台文件系统细节见 [`cross-platform-filesystem.md`](cross-platform-filesystem.md)。
>
> 搜索、模糊匹配与代码高亮的候选依赖见[集成参考](search-preview-integration.md)（[HTML 阅读版](search-preview-integration.html)）；它们尚未集成，不计入下方已实现能力。

图例：✅ 已实现并验证 ／ ⚠️ 已实现但有明确限制 ／ ❌ 没做

## 能力总览

| 域 | 在 core 的模块 | 状态 | 一句话 |
|---|---|---|---|
| 浏览与目录模型 | `model` `watch` `volumes` `path_input` `query` `format` | ✅ | 条目、排序过滤、监听、卷、路径输入、展示格式 |
| 搬运与撤销 | `ops` `resume` `kernel` `entry_kind` | ✅ | 复制/移动/回收站/撤销/续传 + 特殊对象语义 |
| 远程 | `remote` `ftp` `sftp` `s3` `offline` | ⚠️ | 四协议 + 离线缓存；只在 macOS 连过真服务器 |
| 搜索与分析 | `search` `analyze` `compare` `diff` | ✅ | 后台搜索、查大小/重复/校验和、目录比较、内容比较 |
| 归档与预览 | `archive` `preview` | ✅ | zip 读写、预览内容判定与读取 |
| 组织 | `bulk_rename` `marks` `favorites` | ✅ | 批量重命名、标签与颜色、收藏（与 GUI/NSH 共用一份 `settings.json`） |
| 系统集成 | `instance` `logfile` `session` `restore` `terminal` `cloud` | ⚠️ | 单实例、日志、会话、崩溃现场、外部终端、云盘状态 |
| 文本与入口 | `i18n` `palette` `cli` `clipboard` `contract` | ✅ | 双语文案、命令面板匹配、命令行、剪贴板文本、前端契约 |

---

## 1. 浏览与目录模型

| 模块 | 职责 | 主要入口 | 注意 |
|---|---|---|---|
| `model` | `FileEntry`（名字/大小/时间/类型）、`Listing`（一次目录读取 + 排序 + 过滤）、`Sort` / `SortKey` / `ViewMode` | `Listing::read`、`from_entries_showing`、`visible_entry`、`total_count` / `visible_count`、`set_query`、`resort` | **`total_count` 是磁盘上的真实总数**，过滤不改它，界面靠 `N / M` 看出过滤生效。排序**目录优先**，不是纯字母序 |
| `watch` | 目录变更监听（`notify`），把原始事件**收敛**成"这个目录该刷新了" | `watch::...`（bursts 合并） | 事件会连发，必须合并；别在 UI 线程里直接读盘 |
| `volumes` | 侧栏「磁盘」区：本机挂载的卷 + 每块盘剩多少，3 秒 TTL 快照 | `detected`、`capacity_of(path) -> Option<Capacity>`、`Capacity::{describe, total, free}`、`is_removable` + `test_guard()` | 不跟随链接/联接点（`entry_kind::is_link_or_junction`）。容量**探不到就是 `None`**（光盘 / 网络共享 / 空读卡器），界面不许编一个 0 出来 |
| `path_input` | 地址栏那句输入 → 该去哪儿（`~`、相对路径、引号、远程地址） | 解析函数 | 纯逻辑，可直接单测 |
| `query` | 一句话 → 条件树，以及"这个文件过不过" | `Query::parse` | 布尔/正则/通配/大小/时间/标签/路径；`refilter` 是**纯内存**，不重读盘 |
| `format` | 展示层格式化：文件大小、时间戳 | `file_size`、`timestamp` | 不涉及界面，纯字符串 |

`contract::Snapshot` 就是从 `Listing` 投影出来的只读视图。

### 1.1 卷容量：三个必须写对的细节

| 细节 | 做法 | 写错的后果 |
|---|---|---|
| 用哪个"块大小" | Unix 走 `statvfs`（macOS 同样可用），**`total = f_blocks * f_frsize`、`free = f_bavail * f_frsize`** | 拿 `f_bsize` 当乘数，容量能差好几倍（`f_frsize` 才是分配单元） |
| "剩多少"是问谁 | 用 `f_bavail`（**调用者**可用），Windows 用 `GetDiskFreeSpaceExW` 的**第一个出参** | 用 `f_bfree`（文件系统总空闲）会给出用户根本用不到的数（根保留块） |
| 探不到时答什么 | `Option<Capacity>`，`total == 0` 也算探不到 | 光盘 / 网络共享 / 空读卡器会显示"可用 0 / 共 0"，比不显示更糟 |

`Capacity::describe()` 一句话出「可用 {free} / 共 {total}」——GUI 侧栏、TUI、新前端共用同一句，别在界面层各拼一遍。
Windows 侧手写 `unsafe extern "system" { fn GetDiskFreeSpaceExW(...) }`，不拉 `windows-sys`（同 `entry_kind` 的取舍）。

## 2. 搬运与撤销（卖点所在）

| 模块 | 职责 | 主要入口 | 注意 |
|---|---|---|---|
| `ops` | 复制/移动/回收站/重命名/新建、字节与条目计数、`Control` 中止手柄、`History` 撤销栈、本地↔远程四向搬运 | `transfer_entries_deciding`（`copy_entries*` / `move_entries*` 是它的整批策略版）、`Conflicts`（整批 / 逐项问）、`trash`、`rename`、`create_folder_named`、`count_bytes`、`count_entries` | **判"名字被占"用 `symlink_metadata`**（`path_occupied`），不能用 `Path::exists()`：后者跟随链接，断链会答 false，重命名会把链接悄悄覆盖 |
| `resume` | 断点续传：`(源,目标)` 账本、暂存文件、8 GB 预算清扫、`Hold` 守卫 | `journal()`、`Journal::hold`、`key`、`staging` | 后台线程 + 共享暂存目录时**写之前先占住**，否则并发清扫会删掉正在写的数据 |
| `kernel` | **撤销栈的唯一持有者** + 事件广播 + 两条偏好（链接 / 重名）+ 搜索与搬运句柄 | `Kernel::{apply, subscribe, history, history_mut, take_history, put_history, link_policy, set_link_policy, conflict_policy, set_conflict_policy, begin_transfer, begin_conflicts, start_search, cancel_search}`、`Answers`、`KernelCommand`、`KernelEvent`；`can_undo` 在 `contract::Core` 上 | 每个订阅者一条 channel（`async-channel` 单消费者）；命令只回 `Ack`，状态变化走事件；搬运与搜索**在别的线程推事件**（先克隆 `Sender`） |
| `entry_kind` | 特殊文件系统对象的统一语义：`EntryKind`（File/Directory/Symlink/Junction/ReparseOther/HardLink/Shortcut/CloudPlaceholder）、`CopyOptions`、`LinkPolicy`、`MetadataRisk` | `probe`、`classify`、`is_link_or_junction`、`should_descend`、`file_identity`、`reparse_target`、`stream_name_of`、`metadata_risk_note` | **`#[cfg]` 只许出现在 `probe()`**，判定规则必须是纯函数 `classify(&Probe)`，三平台规则才能在本机单测。详见下表 |

### 2.1 特殊对象速查

| 对象 | 识别 | 遍历 | 复制默认 | 保留链接 |
|---|---|---|---|---|
| 符号链接 | `symlink_metadata` | 不递归（canonical 防环） | 跟随内容 | 重建链接 |
| 断链 | 跟随 `metadata` 失败 | 不递归 | 报错 | 重建断链本身 |
| 硬链接 | Unix `nlink > 1`；查重复按 `(dev, ino)` 去重 | 按普通文件 | 独立文件 | — |
| Windows junction | `REPARSE_POINT` 位 + Name Surrogate（bit 29） | 不递归 | 跟随内容 | **拒绝重建**（需 `FSCTL_SET_REPARSE_POINT` + 特权），降级并提示 |
| 其它 reparse | 同上、非 N 位 | 不递归 | 跟随内容 | 拒绝，降级并提示 |
| `.lnk` | `HeaderSize 0x4C` + `LinkCLSID` + LinkInfo | 普通文件 | 复制文件 | — |
| Finder 别名 | `book` 魔数（Apple 未公开格式） | 普通文件 | 复制文件 | — |
| `.desktop` | `[Desktop Entry]` + `Exec/URL/Path` | 普通文件 | 复制文件 | — |
| 云占位（Windows） | `OFFLINE` / `RECALL_ON_OPEN` / `RECALL_ON_DATA_ACCESS` | 不递归 | **拒绝**，提示先下载 | — |
| 云占位（macOS） | `st_flags & SF_DATALESS (0x40000000)` | 不递归 | **拒绝**，提示先下载 | — |
| 云占位（旧 iCloud） | 隐藏名 + `.icloud` 结尾 | 不递归 | **拒绝** | — |

**两个绝对不能改回去的做法**：

1. 判目录用 `fs::metadata`（跟随），不是 `symlink_metadata`——后者对目录链接恒 false，会掉进 `copy_file` 打不开目录。
2. 进度条分母（`bytes_local` / `count_local`）与 `copy_entry` **用同一把尺**，否则分母骗人；防环靠 canonical `seen`。

### 2.2 删除：回收站 vs 彻底删除

| 方式 | API | 可撤销 | 用在哪 |
|---|---|---|---|
| 移到回收站 | `ops::trash(paths)` → `Vec<(PathBuf, SystemTime)>`（只要时刻的便捷封装）<br>`ops::delete(paths, DeleteMode::Trash, &clients)` → `DeleteOutcome` | ✅ 能放回原处 | 内置卷 / 固定磁盘 |
| 彻底删除 | `ops::delete(paths, DeleteMode::Permanent, &clients)` | ❌ **不可撤销** | 用户显式要求；可移动介质上唯一允许的 |

- **可移动介质只允许彻底删除**：`volumes::is_removable(path)` 一次 `statfs`（macOS `MNT_REMOVABLE`）/
  一次 sysfs 读（Linux）/ 一次 `GetDriveTypeW`（Windows）判定，不起外部进程。
  **判不出来时按"不是可移动"处理**——宁可让用户走回收站，也不在不知情时执行不可撤销的删除。
- 执行前用 `ops::delete_mode_for(paths)` 算一次策略：任一项在可移动介质上（或远程）→ `Permanent`，
  并把原因回报给界面（确认框要说明"介质上的回收站靠不住"）。
- **彻底删除不进撤销栈**：`DeleteOutcome::trashed_at` 只在 `Trash` 模式的本地路径上填充，
  UI 拿它压 `HistoryEntry::Trashed`；`Permanent` 永远是空的。这是语义，不是遗漏：
  UI 必须在确认框里明说"不可撤销"，不能给用户留"按 Cmd+Z 能回来"的错觉。
- 三条实现纪律：**删链接不删目标**（先 `symlink_metadata` 判类型）、**不跟 junction/reparse**
  （`entry_kind::is_redirect_path`）、**只读文件先补属主写位再删**（`chmod a+w` 会把只读文件
  变成所有人可写，只补 `0o200` 这一位）。
- 远程路径的删除走 `Remote::remove`，与回收站无关（远程没有本机回收站这回事），
  也**不进撤销栈**。
- 界面入口：`Cmd+Backspace` 按介质自动选方式；`Cmd+Shift+Backspace` 强制彻底删除；
  右键菜单两条并列（带省略号的「彻底删除…」还会再确认一次）。

**仍未做**：彻底删除没有进度条与中止（递归删一棵大目录时会安静地跑一会儿）。

### 2.3 新建文件夹与新建文件（模板）

| 能力 | API | 说明 |
|---|---|---|
| 新建文件夹 | `ops::create_folder_named(parent, name)` + `ops::fresh_folder_path(parent)` | 预填「未命名文件夹」「未命名文件夹 2」… |
| 新建文件 | `ops::create_file_from_template(parent, name, &template)` | 内容来自模板；**同名已存在时拒绝，绝不覆盖** |
| 建议文件名 | `ops::fresh_file_path(parent, &template)` | 按模板后缀预填「未命名.md」「未命名 2.md」 |
| 模板清单 | `templates::TEMPLATES`、`templates::template(id)` | 纯数据，界面照它排菜单 |

模板（`crates/wfiles-core/src/templates.rs`）：

| id | 后缀 | 内容要点 | 可执行位 |
|---|---|---|---|
| `plain` | `.txt` | 0 字节 | — |
| `markdown` | `.md` | `# 标题` + 空行 | — |
| `rust` | `.rs` | `fn main` + `println!` | — |
| `python` | `.py` | shebang + `main()` + `__main__` 判断 | ✅ |
| `shell` | `.sh` | `#!/bin/sh` + `set -eu` | ✅ |
| `json` | `.json` | 一对空花括号（缩进好） | — |
| `yaml` | `.yml` | `---` 文档头 | — |
| `html` | `.html` | 标准骨架（`lang` / `meta charset` / `title`） | — |

- **模板内容写死在 core 里，不读磁盘上的模板目录**：用户目录里没有的东西不能成为新建文件的
  前置条件；将来要做"自定义模板"再加，别现在留个半成品目录扫描。
- 可执行位只在 unix 上设（`0o755`），Windows 忽略——脚本新建出来就能直接跑，
  这是"新建脚本"最实际的一半价值。
- 与「新建文件夹」共用同一套命名对话框与校验（`valid_name` + `path_occupied`）。

## 3. 远程
| 模块 | 职责 | 主要入口 | 注意 |
|---|---|---|---|
| `remote` | 连接配置、系统钥匙串存取、统一的 `Remote` trait、`dav://id/子路径` 拼拆、WebDAV 客户端 | `Remote`、`Config`、`parse_location`、`location`、`fake`（test-support） | **`Remote` trait 一律同步**，异步关在协议实现内部（tokio runtime），否则调用方到处 async |
| `ftp` | 明文 FTP | `Remote` 实现 | 控制连接有状态，用 `Mutex` 锁住 |
| `sftp` | SFTP（russh + tokio，`sftp` feature 默认开） | `Remote` 实现 | known_hosts 认公钥，**换了就拒**；密码与私钥两种登录。`russh` 默认**不启用 `rsa` feature**，只支持 ed25519/ECDSA 主机与用户密钥（避开 NSH 的 `pkcs1` 死锁，见 `docs/nsh-client.md` §四.1）；GUI 侧用 `sftp-rsa` feature 把 RSA 支持加回来 |
| `s3` | S3 兼容对象存储 | `Remote` 实现 | SigV4 签名；桶的两种写法；前缀当文件夹 |
| `offline` | 远程目录列表落盘，断网垫底 | `test_guard()` | 坏缓存当没有，不许拦住启动 |

**加新协议**：实现 `Remote` trait + 在 `remote` 的构造处登记。`ops` 的搬运流程不用改。

## 4. 搜索与分析

| 模块 | 职责 | 主要入口 | 注意 |
|---|---|---|---|
| `search` | 后台从根目录往下走，把 `query` 套在每个文件上 | `Search::{walk, spawn}`、`Cancel`、分批送 `Event::{Batch, Done}` | 随时可中止；不跟随链接/联接点；最多 5000 条 |
| `analyze` | 文件夹大小、**重复文件**、校验和 | `duplicates`、`checksums`、`spawn_duplicates`、`DupOptions` | 查重复三道筛子（大小 → 头 4 KB → 全文件）；同一 inode 的硬链接**不是重复** |
| `compare` | 目录比较 + 同步计划 | 比较入口 + 计划 | 不跟随链接/联接点 |
| `diff` | 文件内容三种看法：行 / 行内字符 / 十六进制字节 | 差异计算 | 先判是否文本，二进制转字节视图 |

## 5. 归档与预览

| 模块 | 职责 | 注意 |
|---|---|---|
| `archive` | zip 的读（列出/解压）与写（打包选中项） | 解压路径要防 zip-slip（`..` 逃逸） |
| `preview` | 判断一个文件该怎么预览，并把能预览的内容读出来 | 只做内容层，不含 UI |

## 6. 组织

| 模块 | 职责 | 注意 |
|---|---|---|
| `bulk_rename` | 模板 / 序号 / 正则替换，先预览后执行 | 预览是纯计算，不落地 |
| `marks` | 标签与颜色，按**路径**记（不跟 inode） | 文件被搬走/删掉后标记失效，`drop_missing` 在启动时清 |
| `favorites` | 侧栏收藏（`{path, is_dir}` + `list` / `toggle`） | **只动 `settings.json` 的 `favorites` 一个键**（用 `serde_json::Value` 读改写），否则会抹掉别的模块刚写的设置；名字不存副本，由 `file_name()` 推 |

## 7. 系统集成

| 模块 | 职责 | 注意 |
|---|---|---|
| `instance` | 单实例转发：排他锁 + 门牌 + 一行线格式 | `instance.lock` / `instance.json` **都不许删**（删了会同时出现两个主实例） |
| `logfile` | 本地日志 + 导出 | 从文件管理器启动的进程**没有终端**，`eprintln!` 用户看不到 |
| `session` | 开了哪些标签、各自看哪个目录 | 只有主窗写（`Starting::primary`） |
| `restore` | 崩溃前每栏选中什么、焦点在哪 | 关标签时当场清 |
| `terminal` | 探测本机终端并在指定目录打开 | 纯逻辑；`AUTO` 表示自动挑一个装着的 |
| `cloud` | 云盘同步状态：认同步目录、判单个文件在不在云端 | 只看不改 |

## 8. 文本与前端入口

| 模块 | 职责 | 注意 |
|---|---|---|
| `i18n` | 中文原文 → 译文的纯表；结构化 `Message`（key + args + cause） | 界面层用 `tr(cx, ..)` / `Message::say(cx)`；core 里用 `say_in(locale)`。**同一 key 只能有一条译文** |
| `palette` | 命令面板的匹配与排序 | 纯逻辑 |
| `cli` | 命令行参数解析 | 用法错误也在这儿判，文案走 i18n |
| `clipboard` | 剪贴板**文本的拼装**（复制路径、复制文件名） | 只拼字符串；真正写系统剪贴板在界面层 |
| `contract` | 前端唯一入口：`Command` → `Event` + `subscribe()` | 命令只描述意图；状态变化一律走事件流 |

### 8.1 契约命令全表（新前端能用的一切）

表里写的是**代码里的真实形状**（元组变体就是元组变体）。这一节曾经按"看起来
更整齐"的样子写过（`Open { path }`、`SetLinkPolicy { follow }`），实际根本没有
`Open` 这条命令（进目录是 `Enter(PathBuf)`）——照文档写代码会编译不过。

| `Command` | 干什么 | 对应的 core 调用 |
|---|---|---|
| `Refresh` | 重读当前目录 | `Listing::read` |
| `Enter(PathBuf)` / `Up` | 进目录 / 回上级 | `Listing` |
| `SetSort(SortKey)` | 换排序列（同列翻向） | `Sort::toggled` |
| `SetQuery(String)` | 过滤（纯内存，不动 `total`） | `query::Query` |
| `Search { root, query }` | 从 `root` 往下搜，**立刻返回**，命中走事件流 | `search::Search` + `search::spawn` |
| `CancelSearch` | 叫停正在跑的那趟搜索（幂等） | 丢掉 `search::Cancel` 即叫停 |
| `SetHidden(bool)` | 隐藏项开关 | `Listing::set_show_hidden` |
| `SetLinkPolicy(LinkPolicy)` | 复制链接时跟随还是复制链接本身 | `Kernel`（权威值在这） |
| `SetConflictPolicy(ConflictPolicy)` | 重名了怎么办：保留两者 / 替换 / 跳过（**整批**） | `Kernel`（权威值在这） |
| `Copy { sources, dest }` / `Move { sources, dest }` | 搬运（阻塞到结束，结果走事件） | `ops::transfer_entries_deciding` |
| `Delete { paths, permanent }` | 删除。`permanent=false` 时**仍会**走 `delete_mode_for`——介质上照样彻底删 | `ops::delete` |
| `Rename { from, to }` | 同目录改名 | `ops::rename` |
| `CreateFolder { parent, name }` | 新建文件夹 | `ops::create_folder_named` |
| `CreateFile { parent, name, template }` | 按模板新建文件 | `ops::create_file_from_template` |
| `Undo` | 撤销（唯一撤销栈在 `Kernel`） | `History::pop_undo` |

`apply` 的同步回执只有 `Event::Listing(Snapshot)` / `Event::Failure { message }`
两种；**其余状态变化一律从 `subscribe()` 的事件流读**（`KernelEvent::*`）。

**只读查询全表**（直接开 `Core` 上的方法，**不开 `Command` 变体**）：

| 方法 | 给什么 | 为什么不是命令 |
|---|---|---|
| `preview(&path) -> Preview` | 这个文件的预览形状 | 看内容什么都没改 |
| `volumes() -> Vec<Volume>` | 本机挂着的卷 + 各自容量 | 扫挂载点不进撤销栈 |
| `can_undo() -> bool` | 撤销栈空不空 | 只问"能不能" |
| `undo_label() -> Option<Msg>` | **下一步会撤掉什么**（栈空 `None`） | 只问，不弹栈 |
| `conflict_policy() -> ConflictPolicy` | 当前整批重名策略 | 只读权威值 |

命令会走"操作落地"那条路（进撤销栈、推 `OperationFinished`），还让十几处
`match` 非穷尽。特别提醒 `undo_label()`：`can_undo()` 只回答"能不能"，
撤销按钮光写「撤销」等于让用户闭眼按 Cmd+Z。它转发的是
`History::last_describe()`——GUI 的 `Ops::undo_label()` 用的是同一个函数，
六种 `HistoryEntry` 各说各的（「新建“草稿”」「复制 2 项」…），
**前端不许自己拼第二份**。返回 `Msg` 而不是 `String`：要换语言的前端
`say_in(locale)`，`Display`（`to_string()`）恒给中文。

**预览不是命令，是只读查询**：`Core::preview(&path) -> preview::Preview`。
它跟上面那几条同类。
同步做，一次最多读 256 KB（`preview::Budget` 默认值，要别的预算用
`preview::load_with`）。四种形状：`Directory` / `Opaque(Kind)` / `Text` / `Hex`，
外加 `Unavailable(String)`（系统错误原文，不翻）。`contract` 再导出 `Preview`
`Kind` `TextPreview` `HexPreview` `HexRow`——**语言名（`Some("Rust")`）与十六进制
的排法（`HexRow::format`）都是 core 的知识**，界面不维护第二份后缀表。
图片 / PDF / 音视频只回 `Opaque`：真解码（缩略图）是界面层的事，而且
**判类型绝不 open 文件**（云占位会因此触发下载）。

两条偏好类命令（`SetLinkPolicy` / `SetConflictPolicy`）的形状与别的命令不同：
它们不动磁盘，只改内核里的权威值并广播一条 `*Changed` 事件，前端据此对齐自己的
开关。**问用户的方式归前端**——GUI 在冲突时弹一次窗、把整批的策略发下来，
TUI 可以先问再搬；语义只有一份，在内核里。

重名有**两种粒度**，都在契约里（`ops::Conflicts`）：

- **整批一个策略**（默认）：`SetConflictPolicy` 定下，搬运按它走，中途不打断。
  批量搬一百份时这是对的。
- **逐项问**：调一次 `Core::begin_conflicts() -> kernel::Answers`，下一趟搬运
  每碰到一项重名就推一条 `KernelEvent::ConflictHit { path }`，然后**停在这一项上**
  等 `Answers::answer(policy)`。`path` 是已经拼好的落点（`dest/名字`）——用户要
  判断的是"要不要盖掉那一个"。一问一答、按到达顺序配对，前端不许提前攒答案。

逐项问把搬运变成**要等**的函数（`ops::transfer_entries_deciding` 在冲突点阻塞），
这不是缺点：人回答问题本来就要时间，而 core 的搬运本来就不在界面线程上跑。
两个收尾口必须都在：等待期间盯着中止位（不然用户按了中止还得先把问题答完），
以及应答口全丢了就退回整批策略（不然一次前端崩溃能把工作线程永远挂住）。
`begin_conflicts()` 与 `begin_transfer()` 一样**每趟都要重新调**，并且都要在发
`Copy`/`Move` 之前——搬运期间 `Core` 被 `&mut self` 独占。

新增命令时两边都要补：`contract.rs` 的 `match` 与本文这张表。

**搜索是唯一"结果不在回执里"的命令**：它要走整棵树、可能还要读文件内容，
让 `apply` 等它等于界面卡死。所以回执给的是**当前清单**（纯内存快照，不重读盘，
搜索不改眼前这个目录），命中的分批走事件流：

- `KernelEvent::SearchBatch { hits: Vec<SearchHit> }`：一批命中（攒够 64 条发一批，
  逐条跨线程发会把队列淹了）。`SearchHit` 只带 `path / name / is_dir / size /
  line`——大小与时间的文案由前端用 `format` 渲染，跟列表行同一套路。
- `KernelEvent::SearchDone { visited, matched, truncated, cancelled }`：
  走完、撞到条数上限（`search::MAX_RESULTS = 5000`）、或被叫停都会有这一条。
  **前端在收到它之前不许把"零命中"说成"搜完了"**。
- 再发一条 `Search` 就是换一趟：旧的 `search::Cancel` 被丢掉，旧搜索自己收手
  （`Cancel` 的 `Drop` 干的就是这件事）。
- 结果列表由前端自己攒：搜索结果属于"这次会话的瞬时展示状态"，跟选中项、
  滚动位置同类，core 不留第二份。

搬运**期间**的三样东西不在命令里：

- `KernelEvent::Progress { label, done, total, bytes_done, bytes_total }`：
  每搬完一项、每写完一个数据块一次，前端从事件流里读（不用轮询 core）。
  `bytes_total == 0` 表示数不出来（没权限），前端退化成按条目数显示。
- `KernelEvent::OperationFinished { touched, failed, skipped }`：
  `skipped` 只在重名策略是「跳过」时非零——这些项磁盘上什么都没变，
  前端不该拿它们去刷新目录，但要用它们解释"为什么只搬来一半"。
- `Core::begin_transfer() -> ops::Control`：**在发 `Copy`/`Move` 之前**调，
  返回一个可跨线程的中止手柄（`Arc<AtomicU8>`）。搬运期间 `Core` 被
  `&mut self` 独占，中途想叫停的线程拿不到锁——手柄必须事先取走。
  每次调用换一个新手柄：中止位不可逆，复用会让下一趟一开始就自己停下。
- `Core::begin_conflicts() -> kernel::Answers`：同样**在搬运之前**调，开了口子
  才会逐项问重名（见上一节的两种粒度）；没调就是整批一个策略、一次也不问。

搬运的**结论那句话**由 core 拼：`ops::summarize(&outcome, cut, aborted) -> Msg`
（"拷贝完成 5 项，跳过 2 项"）。放 core 是因为 GUI 状态栏、TUI 一行、新前端回执
都该说同一句话——它不是哪个界面自己的知识。子句按条件出现（跳过 / 失败 /
降级说明），因此各自一条 i18n key，用 `Message::then_clause` 按语言接起来。

---

## 界面层模块（对照，方便知道"这块不归 core"）

| 模块 | 职责 |
|---|---|
| `app` | 应用外壳与全局状态、窗口、浮层、设置卡、命令面板、状态栏 |
| `pane` | 窗格：导航、列表/网格/分列、行级拖放、详情板、窗格状态栏 |
| `ops_entity` | 文件操作的 UI 枢纽：任务队列、进度与速度、冲突、撤销、后台线程调度 |
| `drag` | 拖放的纯判定（拖什么、能不能落、移动还是拷贝、悬停多久展开） |
| `keymap` | 可配置快捷键（命令 id ↔ 按键），依赖 UI 的命令枚举所以留在界面层 |
| `settings` | 用户设置持久化（GPUI 全局槽位 + JSON 原子写盘） |
| `thumbnails` | 缩略图后台解码 + 有上限的缓存 |
| `meta` | 可选元数据列（Git/图片尺寸/时长/EXIF/ID3/PDF 页数） |
| `term` / `term_entity` | 内嵌终端的纯逻辑 / 绘制与 PTY |
| `remotes` | 远程连接注册表、后台列目录与下载、远程编辑回传 |
| `analyze_entity` / `compare_entity` / `diff_entity` / `search_entity` | 各面板实体 |
| `throttle` | 传输限速令牌桶（时间从外面喂，纯算术） |
| `ui_tests` | 无头窗口集成测试（331 条） |

---

## 已知限制（当前版本就到这里）

| 项 | 状态 | 说明 |
|---|---|---|
| Windows / Linux 真机运行 | ❌ | 只有 macOS 验过。Windows 分支做过**三元类型检查**（见 `cross-platform-filesystem.md` 6.1），能编过 ≠ 跑得对 |
| Windows reparse tag / ADS 的**运行时**行为 | ⚠️ | FFI 已实现并编译通过，需真机/CI 验证 |
| xattr / ADS **保留** | ❌ | 现在只在跨卷搬运时**提示**"可能丢"，没有真的复制过去，也没有"保留/放弃"选择框 |
| 契约覆盖搬运进度 / 冲突对话 / 搜索 | ⚠️ | **进度、中止、冲突策略、逐项问冲突、搜索都已接**（`Progress` + `begin_transfer()` + `SetConflictPolicy` + `begin_conflicts()`/`ConflictHit` + `Search`/`SearchDone`）。没接的一件：**保存的搜索**（`session` 只存标签与目录，不存搜索条件）。用户没拍板，先不做 |
| 全局唯一 `Kernel` | ⚠️ | 当前 `Ops` 与每个 `Pane` 的 `Core` 各持一个；"唯一"是每实例内唯一 |
| 插件系统 | 明确不做 | 用户 2026-10-13 否掉，理由见 `ROADMAP.md`：跨平台 ABI 不存在、版本对不上无解、动用户文件就赌掉"可撤销" |

## 加能力时的自检

1. 这段逻辑是跨客户端一致的业务规则，还是客户端表现？**业务规则放 core，窗口/焦点/选中/渲染及专属显示辅助留客户端**；不依赖 GPUI 不代表一定该下沉。语法高亮等边界见[集成参考](search-preview-integration.md)。
2. 放了新状态？问它是不是权威状态——是就进 `Kernel` / `Core`，不要放界面里各存一份。
3. 加了 `#[cfg(target_os)]`？把判定抽成纯函数，`#[cfg]` 只留"去哪儿取数据"。
4. 加了用户可见文案？中文原文进 `i18n.rs` 的 `EN` 表。
5. 加了判据？先故意改坏代码看它红一次。
