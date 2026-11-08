<img src="assets/wfiles.svg" width="96" align="right" alt="WFiles 图标">

# WFiles

**快、清楚、可撤销的跨平台文件管理器**，Rust 2024 + [GPUI Kit](https://crates.io/crates/gpui-kit) 写的原生桌面应用。

双栏 + 标签页、全键盘可操作、破坏性操作都能撤销；十万项的大目录滚动、排序、过滤不掉帧；远程位置（WebDAV / FTP / SFTP / S3）和本地走同一套操作、同一个后台任务队列。

> **0.1.0**：功能清单已按阶段闭合并逐项验证（过程与取舍都在 [`ROADMAP.md`](ROADMAP.md)），但只有 macOS 在真机上验证过——另两个平台的情况见「平台状态」。

## 功能一览

**浏览**

- 双栏 + 标签页：`Cmd+Alt+D` 分栏、`Tab` 换焦点、`F5` / `F6` 拷到 / 移到另一栏、双栏同步浏览（`Cmd+Alt+S`）
- 三种视图：列表 / 网格 / Miller 分列（`Cmd+1/2/3`）；每个目录记住自己的排序与视图
- 面包屑地址栏（可点、可输入路径）、返回 / 前进 / 上一级 / 刷新、隐藏文件开关（`Cmd+Shift+.`）
- Quick Look 预览（`Space`）：图片、文本、代码、二进制；缩略图后台解码，不挡列表
- 侧栏：主页 / 桌面 / 文稿 / 下载、收藏夹（`Cmd+D`）、磁盘、云盘、远程

**操作与撤销**

- 新建文件夹、重命名（`F2`）、复制 / 剪切 / 粘贴；删除走**系统回收站**（`Cmd+Backspace`），可放回原处
- 批量重命名（`Cmd+Shift+R`）：模板 / 序号 / 正则，带预览与撤销
- 后台操作中心：任务排队、暂停 / 继续 / 中止、字节级进度与速度、失败项重试；**中止不留半个文件**，大文件**断点续传**
- 撤销栈（`Cmd+Z`）：搬运、重命名、删除都在里面
- 拖放：拖进拖出、拖到文件夹 / 标签页、悬停自动展开（弹簧文件夹）；系统拖进来只做拷贝

**搜索与分析**

- 搜索（`Cmd+Shift+F`）：布尔 / 正则 / 通配符，按名称、类型、大小、时间、标签、路径组合；内容搜索在后台跑、随时可中止；结果可保存成常用搜索
- 分析面板（`Cmd+Alt+I`）：文件夹大小 / 重复文件 / 校验和
- 目录比较（`Cmd+Alt+B`）与文件内容比较（`Cmd+Alt+Shift+B`）：差异高亮、单向 / 双向同步
- 压缩包：zip 双击进去直接看、一键解压、选中的打包成 zip

**远程**

- WebDAV / FTP / SFTP（含密钥认证）/ S3 兼容对象存储（OSS、COS、MinIO、R2、B2 等）
- 远程与本地之间四向搬运都进同一个任务队列：进度、暂停、撤销与本地一视同仁
- 远程编辑：下载到本地改，保存时按指纹回传；离线缓存让断网时也翻得动目录
- 传输限速（不限 / 1 / 5 / 10 / 20 MB/s）；云盘同步状态（OneDrive、iCloud Drive、Dropbox、Google Drive、坚果云、百度网盘、阿里云盘）

**终端与命令行**

- 系统终端在当前目录打开（`Cmd+Alt+T`）；内嵌终端抽屉（`Ctrl+Alt+T`）里跑真正的 shell
- 命令行：`wfiles <路径>`、`--reveal`、`--new-tab`、`--new-window`、`--lang`、`--help`、`--version`
- **单实例**：第二次启动不自己开窗，把「开到哪儿」交给已经开着的那一扇

**界面与个性化**

- 中文 / English 双语；快捷键全部可改（命令面板 →「快捷键」）
- 命令面板（`Cmd+P`）覆盖全部动作；全键盘导航
- 标签与颜色（`Cmd+Alt+M`）；可选元数据列：Git 状态、图片尺寸、媒体时长、EXIF、ID3、PDF 页数
- 窗口布局、每个目录的视图与排序、会话与崩溃现场都记得住（重启接回原样）
- 后台任务日志，命令面板里一键导出（反馈问题时带上它）

## 平台状态

| 平台 | 状态 |
|---|---|
| macOS | **已在真机验证**（日常开发与使用都在上面） |
| Windows | 代码层同等对待，但**还没在真机编译过** |
| Linux | 同上 |

三个平台同等优先是这个项目的硬规矩：路径按字节走、删除走系统回收站、保留名与非法字符用同一份校验规则。但「同等对待」目前只到代码层——手上没有另外两个平台的构建环境，**在那里真跑起来之前不敢说能用**。这一栏等验过了会更新。

## 给开发者

业务逻辑全在 `crates/wfiles-core/`（不依赖任何界面框架），界面在 `src/`，依赖方向只有「界面 → core」。
想加一个 TUI、或换掉 GUI 框架：从 [`nsh-poc/src/lib.rs`](nsh-poc/src/lib.rs)（第二个前端的桥接层，只认 `contract::Core`）
和 [`docs/architecture.md`](docs/architecture.md) 起步，核心一行不用改。

| 文档 | 内容 |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | 分层怎么切、状态归谁、契约怎么用、加东西动哪里 |
| [`docs/core-capabilities.md`](docs/core-capabilities.md) | 核心能力清单：模块 × 职责 × 入口 × 已知限制 |
| [`docs/cross-platform-filesystem.md`](docs/cross-platform-filesystem.md) | 三平台文件系统约束与特殊对象处理 |
| [`AGENTS.md`](AGENTS.md) | 工程铁律、测试纪律、环境坑（改代码前必读） |
| [`ROADMAP.md`](ROADMAP.md) | 功能范围、阶段进度、明确不做清单 |

## 构建与运行

需要 Rust stable（本项目用 edition 2024）。

```bash
cargo build               # 构建
cargo run                 # 跑起来（开发用）
cargo test                # 测试
cargo clippy --all-targets
cargo fmt --check
scripts/bundle-macos.sh   # 打一个能双击的 WFiles.app（macOS，release；--debug 可选）
```

- macOS 上**别把 `cargo run` 出来的裸二进制当正式环境**：系统不把它当 App（窗口顶不到最前，还会提示「正在后台运行」）。要看真实观感、要试权限相关的路径，用 `scripts/bundle-macos.sh` 打完再 `open` 那个 `.app`。
- 图标是 `assets/wfiles.svg`，由 `scripts/make-icon.sh` 渲染打包（bundle 时自动调用）。
- 版本号只有一个来源：`Cargo.toml`。

### 安装（macOS）

`scripts/bundle-macos.sh --out <目录>` 打出 `WFiles.app`，拖进「应用程序」即可。包只做了 ad-hoc 签名（没有开发者证书）：本机自用没问题；拷给别人时，对方首次打开需要右键 →「打开」。

## 数据放在哪

| 内容 | 位置 |
|---|---|
| 设置、会话、崩溃现场 | macOS `~/Library/Application Support/WFiles/`；Linux `~/.local/state/WFiles/`；Windows `%APPDATA%\WFiles\` |
| 后台任务日志 | macOS `~/Library/Caches/WFiles/logs/`；Linux `~/.cache/WFiles/logs/`；Windows `%LOCALAPPDATA%\WFiles\logs\` |
| 换个地方存 | 环境变量 `WFILES_SESSION_DIR`（便携使用、多开、测试隔离都用它） |

远程连接的密码只进系统钥匙串（macOS 钥匙串访问 / Windows 凭据管理器 / Linux Secret Service），不写进上面这些文件。

## 常用快捷键

macOS 上 `Cmd` 就是 ⌘；Windows / Linux 上读作 `Ctrl`。全部命令都能改（命令面板 →「快捷键」），下表是出厂键。

| 快捷键 | 动作 | 快捷键 | 动作 |
|---|---|---|---|
| `Cmd+P` | 命令面板 | `Space` | 预览（Quick Look） |
| `Cmd+T` / `Cmd+W` | 新标签 / 关标签 | `Cmd+R` | 刷新 |
| `Cmd+Alt+D` | 双栏开关 | `Tab` | 两栏间换焦点 |
| `F5` / `F6` | 拷到 / 移到另一栏 | `Cmd+1/2/3` | 列表 / 网格 / 分列 |
| `Cmd+C/X/V` | 复制 / 剪切 / 粘贴 | `Cmd+Z` | 撤销 |
| `F2` / `Cmd+Shift+R` | 重命名 / 批量重命名 | `Cmd+Backspace` | 移到回收站 |
| `Cmd+Shift+N` | 新建文件夹 | `Cmd+D` | 收藏当前目录 |
| `Cmd+Shift+F` | 搜索 | `Cmd+Shift+.` | 显示隐藏文件 |
| `Cmd+Alt+I` | 分析面板 | `Cmd+Alt+M` | 标签与颜色 |
| `Cmd+Alt+B` | 目录比较 | `Cmd+Alt+Shift+B` | 文件内容比较 |
| `Cmd+Alt+T` | 系统终端 | `Ctrl+Alt+T` | 内嵌终端 |
| `Cmd+B` | 侧栏显隐 | `Cmd+Alt+O` | 详情板显隐 |

上一级（`Backspace` / `Cmd+↑`）、扩选（`Shift+↑/↓`）、全选（`Cmd+A`）这些也都在。

## 许可

[MIT](LICENSE)。
