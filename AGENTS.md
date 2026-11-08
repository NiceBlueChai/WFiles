# AGENTS.md

给 AI Agent 和新协作者看的仓库操作手册。**改代码前必读。**

## 项目一句话

WFiles：Rust 2024 + GPUI Kit 0.7 的跨平台文件管理器（macOS / Windows / Linux 三平台同等优先），核心卖点**快、清楚、可撤销**。P0–P5 的清单已闭（P5 只剩两项不做：安装包与自动更新要签名与公证、原生菜单/分享/通知 GPUI 没有接口，理由见 `ROADMAP.md` 的「P5 剩下的两项为什么没做」）；下一阶段（P6）待定。**阶段标记会过期，以 `ROADMAP.md` 的 ✅ 为准。**

## 文档地图（先看哪一份）

| 你想知道 | 看 |
|---|---|
| 代码该写在哪一层、状态归谁、加东西动哪里 | [`docs/architecture.md`](docs/architecture.md) |
| 核心已经有什么能力、入口函数叫什么、有哪些限制 | [`docs/core-capabilities.md`](docs/core-capabilities.md) |
| 文件名/路径/回收站/链接/元数据的三平台约束 | [`docs/cross-platform-filesystem.md`](docs/cross-platform-filesystem.md) |
| NSH（TSX）客户端的结构、已接能力与坑 | [`docs/nsh-client.md`](docs/nsh-client.md) |
| 做不做某个功能、阶段进度 | [`ROADMAP.md`](ROADMAP.md)（**以其中的 ✅ 为准，本文件的阶段概述会过期**） |
| 工程铁律、测试纪律、环境坑 | 本文件下面几节 |

## 环境与命令

```bash
cargo build          # 构建
cargo run            # 运行（当前仅验证 macOS）
cargo test           # 全量（两个 crate；GUI 集成测试要 headless，慢）
cargo test -p wfiles-core                  # 只跑核心：单测 + 边界 + 契约（快，改 core 时常用）
cargo test -p wfiles --bin wfiles -- --skip cmd_backspace_trashes_the_selection_and_undo_puts_it_back
                                           # 只跑 GUI；那条 skip 是 macOS Finder 自动化权限，与本项目代码无关
cargo test --release perf:: -- --nocapture # 性能基准（看数字必须 release）
cargo clippy --workspace --all-targets
cargo fmt --check
scripts/bundle-macos.sh   # 打一个能双击的 WFiles.app（release；--debug 可选）
scripts/make-icon.sh      # 从 assets/wfiles.svg 生成 WFiles.icns（bundle 时自动调用）
scripts/smoke-tui.sh      # 用管道真跑一遍 TUI 示例（6 条冒烟判据，改契约时一起跑）
scripts/smoke-nsh-tui.sh  # NSH 终端后端：真 pty 跑 `nsh run --target tui`，回放最后一屏判 10 条（7 静态 + 3 键盘）

cd nsh-poc && ~/.nsh-source/bin/nsh check  # NSH 客户端：类型检查（首次 40~45s，增量 1-3s）
cd nsh-poc && ~/.nsh-source/bin/nsh test   # NSH 客户端：headless 测试（首次 ~1m25s，热 30~50s）
                                           # `nsh` 不在 PATH，别写成 `nsh check`
```

- **NSH 有两份 CLI**：`~/.nsh-source/bin/nsh` 是 0.4.0（源码构建，**只有它带
  `--target tui`**），`~/.nsh/bin/nsh` 是发行版 0.2.0。本仓按 0.4.0 写，见
  [`docs/nsh-client.md`](docs/nsh-client.md) §六。app 的 `nsh-core` / `nsh-base`
  按 **path** 依赖源码 checkout，不是 crates.io——`nsh-tui-host` 与
  `nsh-platform-tui` 没发布，走 registry 就编不出 tui host。
  **这条上游缺口 2026-11-08 拍板不追**（不报 issue、不 fork），path 依赖是长期形态；
  同理 `nsh test --target tui` 不认带 `#[native_module]` 的 app，TUI 判据因此落在
  `scripts/smoke-nsh-tui.sh`，**以后加判据就往脚本里加，别再提搬进 `nsh test`**。
- **`E0463: can't find crate for X which Y depends on` 先怀疑 exFAT 上的陈旧产物**，
  不是镜像源：`/Volumes/Ventoy` 的 `target/` 会丢/坏 rlib，重跑一次 `cargo build`
  多半就过（`oxc_*` 与 `zerofrom_derive` 两次都是这个）。

- Rust edition **2024**，工具链用 stable，无 nightly 特性。
- `target/` 已 gitignore，禁止提交。
- 三平台同等优先：**任何代码不允许假设只在 macOS 跑**。`#[cfg(target_os = ...)]` 每个平台分支都要能编译。
- macOS 上**别指望 `cargo run` 出来的窗口能正常用**：裸二进制被系统当成
  "后台 App"（窗口顶不到最前，还会弹「正在后台运行」）。要看真实观感、
  要试权限相关的路径（自动化 Finder、完全磁盘访问），就跑
  `scripts/bundle-macos.sh` 再用 `open` 打开那个 `.app`。
- 这个项目可能放在外置卷（如 Ventoy）上：**macOS 会在那里给每个文件
  生成 `._xxx` 资源叉伴生文件**，`.gitignore` 已忽略、代码里按名前缀跳过。
  打 `.app` 时那种文件会让 `codesign` 拒签，脚本里已经先清一遍。
  同一个坑会噎住 `git gc`：git 刚写出来的 pack 旁边立刻多一个
  `._pack-*.idx`，它把那个当索引读，报 `non-monotonic index` 再
  `fatal: unable to mark recent objects`。清掉重跑就行
  （`find .git -name '._*' -delete && git gc --prune=now`），
  但它是写一次补一次，得边清边跑，别指望清一遍就完事。
- **应用图标是 `assets/wfiles.svg`，由 `scripts/make-icon.sh` 用无头 Chrome
  渲成 `.icns`**：macOS 自带的工具里没有 SVG 光栅器（`sips` 读不了 SVG，
  `qlmanage` 会把 SVG 当**文本源码**预览），所以借浏览器。改图标只改 SVG，
  渲出来的 `.icns` 进 `target/icon`，bundle 脚本自动调用并把它写进
  Info.plist。**版本号只有一个来源（Cargo.toml）**：plist 里的那两行由
  bundle 脚本用 plutil 现填，不要手抄。
- **别从 macOS 交叉编译另外两个平台，试过了，走不通。** Windows 侧卡在
  `ring` 的 C 代码要 MSVC 运行库头文件（`assert.h` 找不到），Linux 侧卡在
  `wayland-backend` 的 build script 要编 C。`llvm-rc` 那道门倒是能绕
  （gpui 的 `embed-resource` 只探测 `llvm-rc -V /?` 的 stdout 开头、
  不检查产物，`RC_x86_64_pc_windows_msvc` 指个占位脚本就过去了），
  但绕不过真正的 C 依赖。要真验另外两个平台就上 VM 或 CI。

## 仓库结构

**先说清楚分层**（细节见 [`docs/architecture.md`](docs/architecture.md)、能力清单见 [`docs/core-capabilities.md`](docs/core-capabilities.md)）：
业务逻辑全在 `crates/wfiles-core/`（不依赖任何界面框架），界面在 `src/`，方向只有「界面 → core」。
`src/main.rs` 把 core 的模块 `pub use` 回 `crate::xxx`，所以界面里照旧写 `crate::ops::…`。

### 核心：`crates/wfiles-core/src/`

| 文件 | 职责 |
|---|---|
| `lib.rs` | 模块清单与分层说明（先读这个） |
| `contract.rs` | 前端契约：`Command` → `Event`，`Core` 持有列表与内核；新前端只认它 |
| `kernel.rs` | 操作侧状态机：**撤销栈唯一持有者** + 事件广播 + 链接策略 + 重名策略（整批 / 逐项问） |
| `entry_kind.rs` | 特殊文件系统对象统一语义（链接/联接点/硬链接/快捷方式/云占位）与 Windows FFI |
| `model.rs` | 文件条目与目录内容模型、排序、视图模式 |
| `ops.rs` | 复制/移动/删除/重命名、字节计数、`Control` 中止手柄、`History` 撤销栈、本地↔远程四向搬运 |
| `resume.rs` | 断点续传：`(源,目标)` 账本、暂存文件、预算清扫、`Hold` 守卫 |
| `archive.rs` | 压缩包：看内容、解压、打包 |
| `analyze.rs` | 分析任务的纯逻辑：文件夹大小、重复文件、校验和（可中止、带进度） |
| `compare.rs` | 目录比较的纯逻辑：差异清单 + 同步计划（拷哪些/删哪些/哪些不敢动） |
| `diff.rs` | 文件内容比较的纯逻辑：读文件判文本、行级/字符级/字节级差异 |
| `search.rs` | 后台搜索的纯逻辑：走目录、读文本、分批送结果、随时可中止 |
| `query.rs` | 搜索查询：一句话 → 条件树，以及"这个文件过不过" |
| `bulk_rename.rs` | 批量重命名规则与预览（纯逻辑） |
| `marks.rs` | 标签与颜色：一个路径上有什么记号，以及持久化格式（纯逻辑） |
| `palette.rs` | 命令面板的匹配与排序（纯逻辑） |
| `path_input.rs` | 地址栏那句输入 → 该去哪儿 |
| `preview.rs` | Quick Look 内容层：类型判定、文本/二进制读取（纯逻辑） |
| `format.rs` | 展示层格式化（大小、时间戳） |
| `i18n.rs` | 翻译表 + 结构化 `Message`（**纯函数**，无 GPUI） |
| `clipboard.rs` | 剪贴板文本的拼装（复制路径/文件名）；真正写系统剪贴板在界面层 |
| `cloud.rs` | 云盘同步状态：认同步目录、判单个文件在不在云端；只看不改 |
| `remote.rs` | 远程公共部分：连接配置、钥匙串、统一的 `Remote` trait、WebDAV、`dav://id/子路径` 拼拆 |
| `ftp.rs` | FTP 客户端（明文，不带 TLS） |
| `sftp.rs` | SFTP（russh + tokio）：known_hosts、密码与私钥登录 |
| `s3.rs` | S3 兼容对象存储（SigV4、桶的两种写法、前缀当文件夹） |
| `offline.rs` | 离线缓存：远程目录列表落盘/读回 |
| `terminal.rs` | 系统终端探测与启动（纯逻辑） |
| `volumes.rs` | 本机挂着的卷：三平台扫描落点、3 秒 TTL 快照 |
| `watch.rs` | 文件系统监听（notify），把原始事件收敛成"该刷新了" |
| `session.rs` | 会话持久化（含状态目录定位规则） |
| `restore.rs` | 崩溃恢复现场：每栏选中了什么、焦点在哪 |
| `instance.rs` | 单实例转发：排他锁、门牌、一句话线格式 |
| `logfile.rs` | 本地日志（从文件管理器启动的进程没有终端，必须写文件） |
| `cli.rs` | 命令行参数（`--reveal` / `--new-tab` / `--new-window` / `--lang`） |
| `perf.rs` | 性能基准（**test-only**，看数字要用 `--release`） |

### 界面：`src/`

| 文件 | 职责 |
|---|---|
| `main.rs` | 入口：application 初始化 + 开窗；把 core 模块 re-export 回 `crate::xxx` |
| `app.rs` | `WFilesApp` 应用外壳与全局状态；`Starting` 说明这扇窗是谁（主窗/副窗）——会话、窗口布局、崩溃恢复现场只有主窗写 |
| `pane.rs` | 面板系统（导航、列表与网格 UI、视图模式、行级拖放、详情板） |
| `ops_entity.rs` | 文件操作的 UI 枢纽：任务队列、进度与速度、冲突、撤销栈；远程在主线取客户端再交后台 |
| `drag.rs` | 拖放的纯判定（拖什么、能不能落、移动还是拷贝、悬停多久展开） |
| `keymap.rs` | 可配置快捷键：命令 id ↔ 按键、出厂键、校验（依赖 UI 命令枚举，所以留界面层） |
| `settings.rs` | 用户设置持久化（GPUI 全局槽位 + JSON 原子写盘） |
| `thumbnails.rs` | 缩略图：后台解码 + 有上限的缓存 |
| `meta.rs` | 可选元数据列：六种探测的纯逻辑 + 缓存实体 |
| `term.rs` / `term_entity.rs` | 内嵌终端的纯逻辑 / 实体（PTY、量格子、收键盘） |
| `remotes.rs` | 远程连接注册表实体；远程编辑：下载→监听→按指纹回传 |
| `analyze_entity.rs` / `compare_entity.rs` / `diff_entity.rs` / `search_entity.rs` | 各面板实体 |
| `throttle.rs` | 传输限速令牌桶（纯算术，时间从外面喂） |
| `i18n.rs` | GPUI 适配层：`tr(cx)` / `tr_args(cx)` / `Message::say(cx)`（`Say` 扩展 trait） |
| `ui_tests.rs` | 无头窗口 UI 集成测试 |

### 其他

| 路径 | 职责 |
|---|---|
| `crates/wfiles-core/tests/boundary.rs` | 护栏：core 源码/依赖表不许出现界面框架，也不许反向依赖界面模块 |
| `crates/wfiles-core/tests/contract.rs` | 新前端验收场景（浏览 / 复制撤销 / 删除 / 改名 / 新建 / 链接策略 / 重名两种粒度 / 中止 / 搜索 / 预览 / 卷 / 撤销标签，24 条） |
| `examples/wfiles-tui.rs` | 只用标准库 + core 的 TUI 示例：**新增前端的最小接入形态**。覆盖契约三种形状（普通命令 / 只读查询 / 流式搜索）；冒烟判据见 [`scripts/smoke-tui.sh`](scripts/smoke-tui.sh) |
| `nsh-poc/` | **NSH 客户端**：TSX 界面 + `wfiles-core` 后端（独立 `[workspace]`，不在 cargo workspace 里）。结构与坑见 [`docs/nsh-client.md`](docs/nsh-client.md) |
| `scripts/bundle-macos.sh` / `make-icon.sh` | 打 `.app` / 由 SVG 生成 `.icns` |
| `scripts/smoke-tui.sh` | 用管道喂命令**真跑一遍 TUI**，逐项验输出（6 条）：预览内容、撤销标签、策略回显、搜索命中与收尾、卷命令 |
| `assets/wfiles.svg` | 应用图标源稿 |
| `README.md` / `README.en.md` | 门面：功能一览、平台状态、构建运行、数据位置、快捷键表 |
| `ROADMAP.md` | 产品路线图：功能范围、阶段划分、明确不做清单 |
| `docs/architecture.md` | **架构定位**：分层、状态归谁、契约、扩展点、已知边界 |
| `docs/core-capabilities.md` | **能力清单**：模块 × 职责 × 入口 × 状态（先查这里再动手） |
| `docs/cross-platform-filesystem.md` | 三平台文件系统约束调研 + 特殊对象落地状态 |
| `.agents/skills/` | 项目级 skill 的规范位置（`gpui-kit`、`gpui-kit-design-guides`，改 UI 前必读） |
| `.workbuddy/` | **不在仓库里**（已 gitignore，本地保留）：skill 加载副本与记忆文件。它只扫这个路径且跳过符号链接，本地那份必须是真实目录副本，别用软链 |
| `LICENSE` | MIT；改它别忘了 `Cargo.toml` 的 `license` 字段 |

## 代码铁律（来自 docs/cross-platform-filesystem.md，违反即 bug）

1. **路径一律 `PathBuf`/`OsStr`，禁止 `String` 存路径。** Windows 路径不是合法 UTF-8 是常态。
2. **删除必须走平台 trash API（`trash` crate），禁止自己 mv 进回收站目录。** 自己 mv 会丢元数据，"放回原处"和"可撤销"卖点直接破功。
3. **创建/重命名前先校验目标名**：Windows 保留名（CON/PRN/NUL/COM1…）、非法字符（`< > : " / \ | ? *`）、结尾空格句点（静默剥离，不报错）。校验器必须三平台共用同一份规则表。
4. **超长路径用 verbatim（`\\?\`）路径**，并在 UI 层给用户明确提示，不静默失败。
5. **链接要分两种场景，别混成一条**：**遍历 / 统计 / 查重复不跟随**符号链接与联接点（防环，用 canonical `seen`）；
   **复制默认跟随目标内容**（与 Explorer / Finder 一致），设置里可切成"保留链接"，目标卷不支持时自动降级并提示。
   另外：**判"名字被占"必须用 `symlink_metadata`，不能用 `Path::exists()`**——后者跟随链接，
   断链会答 false，于是重命名会把链接本身悄悄覆盖（不可撤销的数据丢失）。特殊对象的完整规则见
   [`docs/core-capabilities.md`](docs/core-capabilities.md) 的 2.1。
6. **跨卷搬运前扫描元数据丢失风险**（xattr/Finder 标签、Windows ADS、POSIX 权限、创建时间），
   把"可能不保留的类别"并进这次操作的结果提示（`entry_kind::metadata_risk_note`）。
   **只在跨卷时说话、只抽前 20 项、只报用户真在意的类别**：同卷复制不会丢，对每个普通文件都报
   `com.apple.FinderInfo` 会让提示变成噪音（「狼来了」比不提示更糟）。**目前只提示，没有真的保留
   xattr/ADS**，要做得先定"保留/放弃"怎么问。
7. 元数据比较/搜索做 **NFC/NFD 双归一化**，存储和显示保留原始字节。
8. **中止信号不许用 `io::ErrorKind::Interrupted`。** 那个种类的含义是"被信号打断，重试一次就好"，`write_all` / `read_exact` / `io::copy` 都会把它当成该重试的错误——用户按了中止会变成无限重试，一直写到磁盘满。用 `ops::aborted()`（`ErrorKind::Other`），判别走 `ops::is_aborted()`。
9. **凡是"后台线程 + 共享暂存/缓存目录"的组合，写之前先占住。** 文件先建出来、账本后补上，中间那段空窗里并发的清扫会把正在写的数据当垃圾删掉。参照 `resume::Journal::hold()` 的守卫写法（`Drop` 自动放开，不靠手动配对）。**进程内那一半靠这张表，跨进程的另一半只能靠文件系统自己的锁**（`File::try_lock`，见 `resume::being_written`）：用户开两份应用、脚本各调一次都是常态，而那张表出了这个进程谁也看不见。**别按时间猜**——"刚建出来 N 分钟内别删"这类判据在"一趟搬运可以跑几小时、正常传完之前根本没有账本"面前必然猜错。同一把锁也用来选主实例（`instance::claim`），于是状态目录里的 `instance.lock` / `instance.json` **都不许删**：删锁文件会让下一个启动在**新的** inode 上抢到锁，于是同时有两个主实例；删门牌会把**新**主实例刚写下的那份删掉（旧的正在退、新的刚好起来），而新主实例不会重写它——从此再没人转得进来。宁可留一张过期的纸，连不上自然走退路。
10. **测试里改进程级状态（override / 全局根目录 / 环境变量）必须拿锁串行化。** 两个测试各改一份、并行跑就会互相顶掉，症状是"单跑必过、全量并行挂"。统一用 `test_guard()`（`volumes::test_guard()` / `offline::test_guard()`），守卫对象在 `Drop` 里恢复现场。**测试等待后台任务时只等"进行中"的状态，绝不要把"终态"（如 `failed`）也算进去**——那会永远等下去。
11. **`~/.Trash` 只能按已知路径读，不能枚举。** macOS 的 TCC 不给普通进程 `read_dir` 回收站（`Operation not permitted`，shell / launchd 用户代理 / `.app` 三种上下文实测一致），但**按已知路径 `stat` / `rename` 是放行的**。所以「从回收站放回」只能靠拼候选名（见 `ops::trash_candidate_names`），别写 `fs::read_dir`——那条路在真机上必挂，而单测里用临时目录跑却是绿的（临时目录不受 TCC 管），典型的"测试过、用户挂"。
12. **Finder 给回收站里的重名项加的是时间戳后缀，不是编号。** 同一个名字删第二次，它叫 `主干 时.分.秒.扩展名`（例如 `报告 23.17.03.txt`），那个时刻是**移进回收站那一刻**的墙上时钟（实测：把 mtime 改成 2020 年、创建与删除相隔 20 秒，后缀都还是删除当时）。别照直觉写成 `主干 2.扩展名`——那个命名法在真机上采样到 0 次。要拿它做判断，先记着测试纪律里那句**探针比推理可靠**——`osascript -e 'tell application "Finder" to get name of every item of trash'` 能列出真东西（但一次 0.6 秒起，别放进界面线程）。
13. **同一份状态文件只有一个写者。** `session.json`、窗口布局（`settings.json` 的 `layout`）、崩溃恢复现场的清扫（`sweep_restore`）与关闭标签时的清理（`forget_restore`）**只归主窗**（抢到 `instance.lock` 的那一扇），副窗只读（判据见 `app::Starting.primary`）。这些文件各自只有一份，两扇窗都写就是互相覆盖，症状是"重启后标签少了一半"，而那时已经无从查起。判据是"抢到位子没有"，不是"谁先开的"——抢不到位子就一定另有一份活着，它正管着这些文件。**按目录的选中现场（`restore.json` 的条目）两边都写**：两扇窗看同一个目录时只有一份"上次"，后写的说了算——这是已知取舍，别当缺陷修。

## 测试纪律

- **偶发失败（单跑过、全量挂）几乎总是真缺陷**，别当测试脆弱放过。查法是构造一个"必然触发"的最小场景，而不是加重跑次数。
- **断言要钉在真实数据源上，别从回调里猜。** 例：续传断点在暂存文件长度上，不在"第一块报了多少钱"上——后者取决于 socket 缓冲大小。
- **修好一个缺陷之后，回头看一眼它把哪些旧测试的前提弄没了。** 新增的判定会让原本"必然发生"的情形变成"偶尔发生"——
  例：加上"叫停落在最后一个字节上算完成"之后，续传测试那个 20 KB 的文件就可能一趟读完，
  于是"第一趟应该中止"的前提不再成立，测试变成了薛定谔的（表现为偶发失败）。
- **测试数据的规模要配得上被验证的性质。** 要验"必然停在半途"，文件就得大到"半途"离末尾还有好几块
  （续传那个从 20 KB 提到 1 MB）；要验"必然一次读完"，就得用比一个读取缓冲还小的文件。
- **要控制读写块大小，就在写入侧定，别指望 socket/读取缓冲。**
  `std::io::copy` 的缓冲是 8 KB、内核 socket 缓冲又是另一个数，都随机器与调度变。
  假服务器分块写 + 每块 `flush` 才算真的把边界定住（而且分块必须比读取缓冲大，否则客户端一次就把两块一起拿走了）。
- **探针比推理可靠。** 拿不准块大小/时机时，临时插一个 `#[test]` 把每块的字节数 `eprintln!` 出来看一眼，
  比读十遍代码推断快。查完记得撤。
- 新写的测试在**修复前跑一遍**：全绿说明它没钉住任何东西。
- **共享的进程级状态（环境变量、缓存根目录、固定路径）必须有测试隔离，且隔离本身要可重入。**
  - `ui_tests.rs` 的 `ensure_isolated()` 就是为此存在的：隔离现场只建一份、存在 thread-local 里活到测试结束，
    同线程再调直接复用。不可重入的话，"先隔离好写会话、再开窗口"这种最常见的测试写法就是自己等自己。
  - 隔离要放在**所有**测试的必经之路上（`open_app_of_width`），不是让每个测试自己记得调——
    原来只有 22/75 个记得，其余 53 个读写用户真实的 `~/Library/Application Support/WFiles/`，
    只要用户用过一次应用，那些断言就可能在并跑时才挂。
- **测试里比对路径前先确认两边是同一种形式**：`tempfile` 在 macOS 上给 `/var/folders/...`，
  而程序里可能是 `/private/var/folders/...`（前者是符号链接）。按原样字符串比对会看成"功能坏了"，
  其实是两个字符串指向同一个目录。用 `canonicalize` 或者从实体里读回实际路径。
- **布局测试必须真的把窗口开成那个宽度。** `open_in` 内部调的是 `open_app`，
  那里宽度写死 1180——拿它去"测四种窗口宽度"，四次量到的是同一个窗口，
  测试全绿而缺陷照旧。布局测试走 `open_in_at_width`。
- **布局断言要钉在"用户能不能用"上，不是"元素在不在"。**
  压成 1 px 也在窗口内，但用户没法用；画到屏幕外也在渲染树里，但用户点不到。
  判据是**实际 bounds**（`ElementSnapshot::bounds()`），不是元素存不存在。
- **越界判据要看四条边，不只右边。** 居中放置的元素超宽时是**左右同时**溢出
  （实测批量重命名那张 620 px 的卡在 520 px 窗口里 `x=-50`），只查右边会漏掉
  左半边按钮点不到的情况。
- **浮层的遮罩要单独断言。** 卡片没越界不等于遮罩盖对了：Quick Look 挂在窗格上
  时卡片一直在窗格内，但侧栏和标签栏都亮着。
- **量布局要先给元素装观察点**（`.test_support()`），且**元组 id 的整数部分
  是 `usize`**——`("crumb", 0usize)` 而不是 `("crumb", 0)`，后者类型推不出`ElementId`。
- **要求本身做不到时，把它拆成"有路可走"单独一条测试**，别降低判据蒙混过去。
  例：520 px 窗口物理上装不下 590 px 的工具栏，于是拆成两条——900/700 px
  断言"两侧边框收掉后装得下"，520 px 断言"工具栏横向滚动、不越界"。
- **同一个 `step` 里连按两个快捷键，第二个会被吞掉**：第一个触发的
  `cx.notify()` 会重画，第二个键落在新焦点上了。要分两步。
- **GPUI 的测试快照读不到界面上显示的字。** `ElementSnapshot::value()` 的文档原话是
  "not rendered text or pixels"；`label()` 读的是**无障碍名**而不是可见文字。
  量过的三组：带 `tooltip()` 的按钮 `label=None`、纯 `div` 也 `None`、
  只有显式 `aria_label` 的元素有值。所以验文案只有一条路——**给要验的那几处
  补无障碍名**（顺带一提：侧栏品牌标语与段落标题本来就缺，读屏软件念不出来）。
- **面板行的 `aria_label` 只喂标题**，状态标记（"当前"）是另一个元素，
  而且是**按需插入**的——没标记的行压根没有那个元素，"没标记"= 查不到，
  不是查到一个空串。
- **`settle()` 无条件重画，所以验不到"这次重画是谁触发的"。** 摘掉某处的
  `cx.notify()` 测试照样过，因为前一个动作（比如 `close_palette`）自己就 notify，
  顺带把界面刷了一遍，看起来像是这一处生效了。碰到这种情况**别降低判据**，
  写进注释说清"这句 notify 冗余但不能删"以及为什么（下一个调用方不会有前一个动作帮它重画）。

- **新加快捷键前先把已占用的键列出来**（`grep -oE '"(cmd|ctrl|...)-...' src/keymap.rs`）。
  `cmd-alt-i` 是分析面板、`cmd-alt-e` 是编辑远程文件——抢了键的那个功能会
  **永远按不出来**，而且没人会去查另一处绑在哪。
- **测试代码也要过另外两个平台的编译。** `std::os::unix` / `std::os::windows`
  这类路径必须紧跟同名 `#[cfg(unix)]` / `#[cfg(windows)]`——连 `#[test]`
  也一起门掉，Windows 上 `cargo test` 会把测试模块整个编进去。
  这一处漏过一次：`model.rs` 里两个用 `std::os::unix::fs::symlink` 的链接
  测试没加门，macOS 上全绿——**本地编译器看不见的那一半才是会漏的那一半**。
  手上没有另一个平台的编译器时，挡得住的只有静态审计：
  `grep -rn 'std::os\.' src/`、`grep -rn '#\[cfg(target_os' src/` 逐条对
  引用点前面的 `#[cfg`（`cfg(平台)` 门着的项，引用点也必须在门里）。
- **"快捷键还灵不灵"只能靠再按一个键去验**（焦点在不在渲染树里从界面上看不出来）：
  切栏/关栏之后按一下 `cmd-t`，看标签栏多没多一栏。写得再"像"也不如这一下
  （见 `ui_tests.rs` 里 `the_shortcuts_survive_*` 那一组）。

### ui_tests 的几处具体手感（踩过才写下来的）

- **菜单项 id 是序号，会随菜单项增减漂移** → 先读 `label()` 再决定点哪个，别硬编码序号。
  常用稳定 id：`overlay-card` / `prompt-ok` / `confirm-ok` / `bulk-ok` / `favorite` /
  `toggle-hidden` / `status-message` / `pane-status`。
- **prompt 输入框自动聚焦，且 `window.input()` 是从光标处插入** → 想替换全文先 `press("cmd-a")`。
- **`try_find` 不按 `visible` 过滤**（它只在渲染树里找），所以判断"看不看得见"必须自己量 bounds。
- **单栏没有 `pane-slot`** → 用 `("row-name", i)`；双栏用 `row_names_in(slot)`。
- **竖向溢出要单独开那个高度**（上面那条只讲了宽度）：`open_app_of_size(cx, w, h)`——
  只改宽度的测试量不到竖向溢出。点折叠线以下的行之前先 `scroll_sidebar()`；
  滚动容器自己也要 `.test_support()`。可用的开口只有三个：`open_in` /
  `open_in_with_app` / `open_in_at_width`（**没有更多 `open_in_*` 变体**，别照名字猜）。
- **`ViewMode` 是 `rename_all = "snake_case"`** → 夹具里写 `"Grid"` 会让**整份 settings
  解析失败、静默退回默认值**，看着像"设置没生效"。
- **从文件管理器启动的进程没有终端**：`eprintln!` 看不见，要写日志文件。
- **`tempfile::tempdir()` 在 macOS 给 `.tmpXXXX`**，要按真名建子目录做夹具；
  比对路径前先 `canonicalize`（`/var` 与 `/private/var` 是同一个目录两种写法）。

## 架构与范围纪律

- 新功能先查 `ROADMAP.md`：在"明确不做"清单里的，直接拒绝，不要"顺手加上"，也**不要因为"先把接口定好放着"就例外**。
- **用户明确否掉过的东西，不要再提案，也不要"换个名字再提一次"。** 2026-10-13 用户否掉了插件系统（三类扩展点：挂载点 / 查看器 / 自定义列），已写进 ROADMAP 明确不做清单：跨平台 ABI 在 Windows 上不存在、插件与主程序版本对不上是个无解的坑、能改用户文件就赌掉了"可撤销"这个卖点。
- 性能基线是硬指标：10 万项列表不掉帧、缩略图异步、万级目录冷启动 <1s。引入同步 IO 到 UI 线程前先测量。
- **量性能一律用 release。** `cargo test` 默认 debug，不开优化时慢一到两个数量级（实测 10 万项过滤：debug 15 ms / release 3.0 ms；排序 79 ms / 39 ms）。拿 debug 的数字定门槛，等于把门槛定在用户永远碰不到的地方。
  - 看真实数字：`cargo test --release perf:: -- --nocapture`（九条基准自己排队量，读到的是独自跑的数字）
  - debug 门禁照样跑（`cargo test perf::`），预算经 `budget()` 放宽 40 倍，作用是抓"有人手滑写成 O(n²)"。
- **改数据结构时要问"这个操作每次按键都会走吗"。** 例：过滤框每敲一个字母走一次 `set_query`，而它原来把整个目录重新排了一遍——排序键一个字符都没变，那 19 ms 白花，直接吃掉一帧。
- **flex 布局里"谁能压缩"要显式设计，别指望自动。** 三条踩过的：
  - 外层 flex 容器缺 `min_w_0` → 最小宽度默认 `auto`，内容有多宽这栏就要多宽，窗口再窄也不缩（整排以固有宽度向右溢出）
  - 固定宽度的子元素是布局的硬底 → 想让它让位，宽度得写在**可收缩的父槽**上，写在它自己身上等于钉死
  - `h_flex()` 带的是 `items_center`（cross 轴居中），那种情况下遇到超宽内容**不会重新分配**，子元素照旧按固有宽度往右排
- 固定边框（侧栏 / 详情板这类 `w(px(N)) + flex_shrink_0`）在窄窗口下会把内容区挤没。
  给用户折叠开关，别做"窄了自动藏"——用户比程序更清楚自己现在有多宽。
- **竖向是同一个问题的另一半，而且更容易漏。** 侧栏中间那几段（收藏、卷、标签）
  条目数不可控，竖着堆就把页脚顶出窗口；横向的测试一条都测不出竖向溢出。
  做法：中间段装进 `flex_1 + min_h_0 + overflow_y_scroll` 的列，页脚做它的**兄弟**
  钉底（不是"最后一个子元素靠 `justify_end`"）。滚动容器自己也要 `.test_support()`；
  点折叠线以下的行之前先 `scroll_sidebar()`。开那个高度用 `open_app_of_size(cx, w, h)`。
- **布局挤不下时，第一问是"结构对不对"，不是"尺寸够不够"。**
  - 顶部**地址栏一行、工具栏一行**（跟 Win11 Explorer 一样）。两者挤在一行时
    那一排固定部分合计 590 px，比任何一档窗口都宽。
  - 我第一次的修法是**给错误的结构打补丁**：砍按钮（12 → 4）、加横向滚动、
    给筛选框设 `max_w`。三个补丁全在治"一行塞不下"，没问"为什么要塞一行"。
    用户一句「地址栏不应该自己单独在上边吗」把结构改对之后，三个补丁里两个当场作废。
  - **判断法**：数固定部分的和，跟最窄那档窗口比。**差得多说明结构错了**
    （该分行、该折叠），差得少才是尺寸问题。
  - **"塞进一行"是最容易想到也最该被拒绝的招**——它往往就是问题本身。
  - 砍按钮的理由要跟着结构一起改：结构对了之后再说"放不下"是假的，
    真实理由是"一排十二颗里十颗一年按不到两次，只是在稀释那四颗有用的"。
  - **"窗口太窄"也可能是假的**。同一个结构错误会在宽窗口下照样发作：
    批量重命名五个输入框一行需求 698px，而 620px 的卡片只剩 580px 内容宽——
    1180px 的宽窗口里五个框**每个都被削掉十几个像素**（模板 260→212、步长 70→57）。
    判据要落在"这一行的需求 vs 容器的内容宽"，不是"窗口宽不宽"。
  - **深度不由窗口决定**。路径有多少层是用户选的，面包屑那种"内容长度不可控"
    的地方只能折叠（老段收进「…」），压缩救不回来——祖先段压到保底，
    行还是比容器宽，于是**当前目录**被挤出屏幕，而它恰恰是用户最需要看见的那一段。
  - **固定宽度的东西不参与收缩，等于钉死**。`Button` 根节点、`min_w`、
    包在 `flex_shrink_0` group 里的子元素，三者任一都在，行就永远缩不进容器。
    要让它让位，宽度得写在**可收缩的父槽**上。
  - **竖向同理：长度不可控的一列要么自己滚，要么把别的东西顶出去。** 侧栏的
    位置 / 收藏 / 磁盘 / 云 / 远程每一段都会长（收藏是用户加的、磁盘会热插拔），
    所以它们装进一个 `flex_1 + min_h_0 + overflow_y_scroll` 的列，底部的
    「本地文件系统」是**它的兄弟**（钉在底部），多出来的高度自己滚。
  - 这一条最阴的地方在判据上：**没有裁剪祖先时，溢出到窗口外的元素照样报
    `visible`**。所以旧结构（页脚直接排在内容后面）在磁盘行变高之后界面已经残了，
    而"点得到"的判据还是绿的——直到内容装进 `overflow_y_scroll`，那层裁剪才
    让 `window.click` 报 `not visible`，缺陷才现形。**滚动容器顺手把假的绿变回真。**
  - **只在需要时插入的元素会让整列错位**。标记色点原来按需插入，被标记的那行
    就比旁边所有行右移"点宽 + 一个 gap"——表头不补那一段的话「名称」也跟着错。
    这种槽要**整列常驻**，不用时画全透明。
  - **浮层该盖多大，取决于它是谁的事**。Quick Look 是"看一眼整个文件"，
    挂在窗格上就只盖住窗格（实测 520px 窗口里窗格只剩 310px，卡片没法看）——
    盖住整个窗口的东西就该画在应用层。
- **换了活动栏、关掉浮层之后，键盘必须有人接手**（`app::hand_keyboard_to_active_pane`）。
  焦点所属的元素一旦不在渲染树里，gpui 就把按键派发到**窗口根节点**，而
  `on_action` 挂在应用根元素上——两者之间没有路径，于是**应用级快捷键全哑**：
  切栏、关栏、开标签、命令面板、分栏都按不出来，不报错、不留痕，点一下界面又好了。
  会触发它的动作一大把：切栏、关栏、点标签栏、关命令面板、关带输入框的对话框
  （重命名 / 批量重命名 / 连接）。**新加"会换掉焦点所在元素"的动作时，先问一句
  "键盘接下来在谁手上"。**
  - 反过来的那一半同样要守：模态（命令面板、带输入框的对话框）**开着的期间不抢**——
    用户正在里面打字，抢走了连 Esc 都送不到它手上；关掉模态的人才负责还回去。
- 三平台通用 + 日常高频才做；低频、单平台专属、可被系统/专门应用覆盖的能力不做。

## 工作方式（协作者约定）

- **结论先行 + 表格化**，反感冗余铺垫。
- 调研类任务：先给答案，用户确认后再落盘改文件。
- 拿不准的设计决策，列选项 + 给出推荐默认值，交用户拍板。
