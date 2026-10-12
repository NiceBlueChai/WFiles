# AGENTS.md

给 AI Agent 和新协作者看的仓库操作手册。**改代码前必读。**

## 项目一句话

WFiles：Rust 2024 + GPUI Kit 0.7 的跨平台文件管理器（macOS / Windows / Linux 三平台同等优先），核心卖点**快、清楚、可撤销**。当前阶段 P0（技术基础，macOS 可构建）。

## 环境与命令

```bash
cargo build          # 构建
cargo run            # 运行（当前仅验证 macOS）
cargo test           # 测试（dev-deps: tempfile）
cargo clippy --all-targets
cargo fmt --check
```

- Rust edition **2024**，工具链用 stable，无 nightly 特性。
- `target/` 已 gitignore，禁止提交。
- 三平台同等优先：**任何代码不允许假设只在 macOS 跑**。`#[cfg(target_os = ...)]` 每个平台分支都要能编译。

## 仓库结构

| 路径 | 职责 |
|---|---|
| `src/main.rs` | 入口：application 初始化 + 开窗 |
| `src/app.rs` | `WFilesApp` 应用外壳与全局状态 |
| `src/pane.rs` | 面板系统（导航/列表与网格 UI、视图模式、行级拖放） |
| `src/drag.rs` | 拖放的纯判定逻辑（拖什么、能不能落、移动还是拷贝、悬停多久展开） |
| `src/analyze.rs` | 分析任务的纯逻辑：文件夹大小、重复文件、校验和（可中止、带进度） |
| `src/compare.rs` | 目录比较的纯逻辑：差异清单 + 同步计划（拷哪些/删哪些/哪些不敢动） |
| `src/compare_entity.rs` | 比较面板实体：两栏对齐的差异列表、筛选、同步按钮（搬运交给 Ops） |
| `src/diff.rs` | 文件内容比较的纯逻辑：读文件判文本、行级/字符级/字节级差异 |
| `src/diff_entity.rs` | 内容比较面板实体：三种看法、并排列表、Esc/↑↓/Enter |
| `src/keymap.rs` | 可配置快捷键：命令 id ↔ 按键的映射表、出厂键、配置校验（纯逻辑） |
| `src/model.rs` | 数据模型（目录/条目/会话） |
| `src/ops.rs` | 文件操作（复制/移动/删除/重命名）、字节计数与任务控制手柄 `Control` |
| `src/ops_entity.rs` | 文件操作的 UI 枢纽：任务队列、进度与速度、冲突、撤销栈 |
| `src/palette.rs` | 命令面板的匹配与排序（纯逻辑） |
| `src/preview.rs` | Quick Look 内容层：类型判定、文本/二进制读取（纯逻辑） |
| `src/thumbnails.rs` | 缩略图：后台解码 + 有上限的缓存 |
| `src/bulk_rename.rs` | 批量重命名规则与预览（纯逻辑） |
| `src/settings.rs` | 用户设置持久化（默认终端、每目录的排序与视图模式、窗口布局） |
| `src/terminal.rs` | 系统终端探测与启动（纯逻辑） |
| `src/meta.rs` | 可选元数据列：六种探测的纯逻辑 + 缓存实体（后台线程、按 mtime 失效） |
| `src/marks.rs` | 标签与颜色：一个路径上有什么记号，以及它的持久化格式（纯逻辑） |
| `src/query.rs` | 搜索查询的纯逻辑：一句话 → 条件树，以及"这个文件过不过"的判定 |
| `src/remote.rs` | 远程位置的纯逻辑：连接配置、钥匙串存取、WebDAV 客户端、`dav://id/子路径` 的拼拆 |
| `src/search.rs` | 后台搜索的纯逻辑：走目录、读文本、分批送结果、随时可中止 |
| `src/search_entity.rs` | 搜索面板实体：输入 → 查询、结果列表、Enter 跳到那个文件 |
| `src/term.rs` | 内嵌终端的纯逻辑：按键 → PTY 字节、屏幕与回滚、选中与取文本 |
| `src/term_entity.rs` | 内嵌终端实体：起 PTY、跨线程收输出、量格子画屏幕、收键盘与鼠标 |
| `src/clipboard.rs` | 剪贴板 |
| `src/format.rs` | 展示层格式化 |
| `src/session.rs` | 会话持久化（含状态目录的定位规则） |
| `src/remotes.rs` | 远程连接注册表实体：客户端、目录缓存、后台列目录与下载（界面线程不做网络） |
| `src/watch.rs` | 文件系统监听（notify） |
| `src/ui_tests.rs` | 无头窗口 UI 集成测试 |
| `ROADMAP.md` | 产品路线图：功能范围、阶段划分、明确不做清单 |
| `docs/cross-platform-filesystem.md` | 三平台文件系统约束调研（文件名/路径/回收站元数据） |

## 代码铁律（来自 docs/cross-platform-filesystem.md，违反即 bug）

1. **路径一律 `PathBuf`/`OsStr`，禁止 `String` 存路径。** Windows 路径不是合法 UTF-8 是常态。
2. **删除必须走平台 trash API（`trash` crate），禁止自己 mv 进回收站目录。** 自己 mv 会丢元数据，"放回原处"和"可撤销"卖点直接破功。
3. **创建/重命名前先校验目标名**：Windows 保留名（CON/PRN/NUL/COM1…）、非法字符（`< > : " / \ | ? *`）、结尾空格句点（静默剥离，不报错）。校验器必须三平台共用同一份规则表。
4. **超长路径用 verbatim（`\\?\`）路径**，并在 UI 层给用户明确提示，不静默失败。
5. **软链接默认不跟随**，UI 显示链接目标；行为要与三平台系统文件管理器对齐。
6. **跨 OS 复制前扫描元数据丢失风险**（xattr/Finder 标签、Windows ADS、POSIX 权限、创建时间），丢之前问用户。
7. 元数据比较/搜索做 **NFC/NFD 双归一化**，存储和显示保留原始字节。

## 架构与范围纪律

- 新功能先查 `ROADMAP.md`：在"明确不做"清单里的，直接拒绝，不要"顺手加上"。
- 性能基线是硬指标：10 万项列表不掉帧、缩略图异步、万级目录冷启动 <1s。引入同步 IO 到 UI 线程前先测量。
- 三平台通用 + 日常高频才做；低频、单平台专属、可被系统/专门应用覆盖的能力不做。

## 工作方式（协作者约定）

- **结论先行 + 表格化**，反感冗余铺垫。
- 调研类任务：先给答案，用户确认后再落盘改文件。
- 拿不准的设计决策，列选项 + 给出推荐默认值，交用户拍板。
