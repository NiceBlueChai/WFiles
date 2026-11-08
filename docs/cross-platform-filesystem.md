# 跨平台文件系统约束清单（macOS / Windows / Linux）

> 调研日期：2026-10-12。范围：macOS（APFS / HFS+ / exFAT）、Windows（NTFS / exFAT / FAT32）、Linux（ext4 / btrfs / xfs）。
> 用途：WFiles `core` / `fs` / `platform` 三层的设计硬约束。所有结论附来源，实施前以目标机器实测为准。

## 1. 长度限制（注意单位不同）

| 平台 / 文件系统 | 单组件上限 | 完整路径上限 | 计量单位 |
|---|---|---|---|
| Windows NTFS / exFAT | 255 | 260（MAX_PATH，含盘符、分隔符与结尾 NUL） | **UTF-16 code units** |
| Windows 长路径（Win10 1607+） | 255 | ~32,767（`\\?\` 前缀，前缀本身也计入） | UTF-16 code units |
| macOS APFS / HFS+ | 255 | 1,024（PATH_MAX） | **UTF-8 字节** |
| Linux ext4 / btrfs / xfs | 255 | 4,096（PATH_MAX） | **字节** |
| FAT32 | 255（VFAT LFN） | 32,760 | UCS-2 |
| exFAT | 255 | 32,760 | UTF-16 |

关键差异：

- **单位不统一**：Windows 数 UTF-16 code unit，Linux/macOS 数字节。一个 255 字符的中文名在 Windows 合法（255 units），在 Linux 是 765 字节直接超限。
- **APFS 格式本身允许 1022 字符名**，255 是 macOS 强制的；同理 NTFS 无 260 限制，260 是 Win32 API 的历史包袱。
- Windows 长路径三件套缺一不可：① 系统开关 `LongPathsEnabled`（Win11 25H2 起在「设置 › 系统 › 高级 › 文件资源管理器」有开关）② 应用 manifest 声明 `longPathAware` ③ 调用方用 `\\?\` 前缀。**资源管理器自身至今不支持超 MAX_PATH 路径**。
- 超限行为不一致：Windows 组件超长有时静默截断而非报错；深层嵌套超路径直接拒绝创建（"目标路径太长"）。

## 2. 非法字符与保留名

| 平台 | 分隔符 | 非法字符 | 保留名 | 结尾字符 |
|---|---|---|---|---|
| Windows | `\` | `< > : " / \ | ? *` + 控制字符 0–31 | CON、PRN、AUX、NUL、COM0–COM9、LPT0–LPT9、CONIN$、CONOUT$，以及 COM¹/²/³、LPT¹/²/³（上标数字同样保留） | 空格、句点 |
| macOS | `/` | POSIX 层禁 `/` 与 NUL；`:` 文件系统允许存但 Finder 展示为 `/` | 无 | 无限制 |
| Linux | `/` | 仅禁 `/` 与 NUL | `.` 与 `..` | 无限制 |

要点：

- **Windows 保留名带扩展名也非法**：`NUL.txt`、`COM1.log`、`aux.tar.gz` 都会被当作设备名解析（大小写不敏感）。Windows 11 起只有显式 `\\.\CON` 形式才当设备，但通过 `\\?\` 原始 API 创建的同名文件常规工具无法访问——**创建时仍必须拦截**。
- **结尾空格/句点被静默剥离**：Win32 路径规范化会移除结尾所有 `.` 和空格，用户输入"报告 v2."实际建成"报告 v2"，不报错。
- 前导 `.`：macOS/Linux 是隐藏约定；Windows 用 hidden 属性（前导 `.` 也有效果）。
- 前导 `-`：Linux 合法但会破坏命令行工具（需 `./-rf` 或 `--` 才能操作）。

## 3. 大小写与 Unicode 规范化

| 平台 | 大小写 | Unicode 规范化 |
|---|---|---|
| Windows NTFS | 默认不敏感、保留原始大小写；Win10 1803+ 可**按目录**开启敏感标志（`fsutil file setCaseSensitiveInfo`） | 不规范化，原样存储 |
| macOS APFS | 默认不敏感、保留大小写（可格式化敏感卷） | **不改变存储形式**，但比较时大小写 + 规范化均不敏感（哈希实现）；HFS+ 历史行为是存储时规范化为 **NFD** |
| Linux ext4 | 敏感（字节比较） | 不规范化；ext4 5.x 起支持按目录 casefold，默认关闭 |

工程含义：

- NFC/NFD 在 Linux 上是两个文件；从 macOS 拷出的 NFD 名字，在 Linux 上用 NFC 字符串**搜不到也匹配不上**。
- 复制到大小写不敏感卷前必须检测冲突：`File.txt` vs `file.txt` 在 Windows/macOS 上会碰撞。
- **WFiles 策略**：存储与显示保持原字节；比较、搜索、去重时做 NFC + NFD 双归一化匹配。

## 4. 编码与 Rust 实现要点

- **Linux 文件名是任意字节序列，不保证合法 UTF-8**：全程 `OsStr` / `PathBuf`，禁止 `String::from_utf8` 假设；UI 显示用 lossy 替换并保留原始字节视图。
- Windows 是 UTF-16（Rust 内部 WTF-8）：校验 255 上限时按 UTF-16 code unit 计数，不是 char 数。
- macOS 是 UTF-8：校验 255 按字节数。
- **Rust std::fs 不会自动绕过 MAX_PATH**：需自行转 `\\?\` verbatim 路径（`canonicalize` 返回值自带该前缀可复用）；发布 manifest 声明 `longPathAware`。

## 5. 元数据载体差异（跨平台复制丢失矩阵）

| 元数据 | macOS | Windows | Linux |
|---|---|---|---|
| 扩展属性 | xattr：FinderInfo、资源叉 `com.apple.ResourceFork`、标签 `com.apple.metadata:_kMDItemUserTags`（二进制 plist）；macOS 26 起支持文件夹颜色 + emoji | NTFS ADS：`Zone.Identifier`（网络下载标记）等流数据 | `user.*` xattr |
| 标签/颜色 | Finder 标签（xattr） | 无对应概念 | Dolphin 用 `user.xdg.tags` |
| 下载来源标记 | `com.apple.quarantine` | `Zone.Identifier` ADS | 无 |
| 权限 | POSIX rwx + ACL + BSD flags（uchg 锁定） | 只读属性 + NTFS ACL（与 POSIX ACL 不可互转） | POSIX rwx + ACL |
| 创建时间 | `st_birthtime`（APFS/HFS+） | CreationTime（NTFS） | `statx STATX_BTIME`（ext4 4.11+ / btrfs / 新内核 xfs） |

工程含义：

- 跨 OS 复制、复制到 exFAT/FAT32：**xattr、ADS、POSIX 权限、创建时间全部丢失**。复制前应扫描并提示"将丢失 Finder 标签 / 下载标记 / 权限"，提供"保留元数据"选项（macOS ↔ Linux 可用 xattr 保留；ADS 不可移植）。
- 权限跨平台不可映射：NTFS ACL ↔ POSIX ACL 是两套模型，跨平台同步时按"目标平台重建"处理并在 UI 明示。

## 6. 链接与特殊文件

| 平台 | 符号链接 | 联接点 | 硬链接 | 其他 |
|---|---|---|---|---|
| macOS | 支持（文件/目录）；另有 Finder 别名 alias | 无 | 支持 | — |
| Windows | 需管理员或开发者模式（`SeCreateSymbolicLinkPrivilege`） | junction（仅目录，需管理员） | NTFS 同卷支持 | reparse points：OneDrive Files On-Demand、WSL、Git symlink |
| Linux | 标准支持 | 无 | 标准支持 | — |

复制语义坑：Explorer 复制 symlink **默认跟随目标内容**（rsync `-a` 则保留链接）；复制到 exFAT/FAT32 无法保留链接（自动降级为跟随）。
**WFiles 策略**：默认跟随（避免悬空链接），设置里可切换"保留链接"；目标文件系统不支持时自动降级并提示。

### 6.1 落地状态（2026-11，`crates/wfiles-core/src/entry_kind.rs`）

统一语义层 `EntryKind`：`File` / `Directory` / `Symlink{target,broken}` /
`Junction{target}` / `ReparseOther{tag}` / `HardLink{links}` /
`Shortcut{flavor,target}` / `CloudPlaceholder`。界面、遍历、搬运、统计只认它，
`#[cfg]` 只出现在 `probe()` 里。

| 对象 | 识别依据（官方） | 遍历 | 复制默认 | 保留链接 |
|---|---|---|---|---|
| 符号链接 | `symlink_metadata` | 不递归（canonical `seen` 防环） | 跟随目标内容 | 重建 symlink |
| 断链 | `metadata` 失败 | 不递归 | 复制失败并报错 | 重建断链本身 |
| 硬链接 | Unix `nlink > 1` | 按普通文件 | 复制成独立文件 | — |
| Windows junction | `FILE_ATTRIBUTE_REPARSE_POINT` + Name Surrogate 位 | 不递归 | 跟随内容 | **不重建**（需 `FSCTL_SET_REPARSE_POINT` + `SE_CREATE_SYMBOLIC_LINK_NAME`），降级并提示 |
| 其它 reparse | 同上，非 N 位 | 不递归 | 跟随内容 | 不重建，降级并提示 |
| `.lnk` | `HeaderSize 0x4C` + `LinkCLSID` + LinkInfo | 普通文件 | 复制文件 | — |
| Finder 别名 | `book` 魔数（**Apple 未公开格式**） | 普通文件 | 复制文件 | — |
| `.desktop` | `[Desktop Entry]` + `Exec/URL/Path` | 普通文件 | 复制文件 | — |
| 云占位（Windows） | `OFFLINE` / `RECALL_ON_OPEN` / `RECALL_ON_DATA_ACCESS` | 不递归 | **拒绝**，提示先下载 | — |
| 云占位（macOS） | `stat.st_flags & SF_DATALESS (0x40000000)` | 不递归 | **拒绝**，提示先下载 | — |
| 云占位（旧 iCloud） | 隐藏名 + `.icloud` 结尾（Apple 未文档化） | 不递归 | **拒绝** | — |

三个刻意选择：

1. **判"名字被占"用 `symlink_metadata` 不用 `Path::exists()`**。后者跟随链接，
   断链会答 false，于是重命名会把链接本身悄悄覆盖掉（不可撤销的数据丢失）。
2. **进度条分母与 `copy_entry` 同一把尺**：目录链接真的会被复制两遍，分母就算两遍；
   canonical `seen` 挡住指回祖先的链接，不会数成无限大。
3. **macOS 判 dataless 只读 flags**。Apple TN3150 明确 `fileExists` / 枚举目录都可能
   触发 materialization（联网下载）；读 `st_flags` 不触发，读内容才触发。
4. **查重复按 `(dev, ino)` 去重**。同一 inode 的多个硬链接不是「重复文件」——
   删掉一个不释放任何空间，报出来只会引导用户白删。Windows 上拿这个身份要
   每个文件开一次句柄，对"扫几万个文件"太贵，那边 `file_identity` 返回 `None`：
   宁可多报，也不把两份真数据当成一份。

#### Windows 侧的 FFI（`entry_kind.rs` 的 `win32` 模块）

手写 `extern "system"`，不引入 `windows-sys`：只用 kernel32 的 6 个函数。

- `CreateFileW(..., FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)`
  + `DeviceIoControl(FSCTL_GET_REPARSE_POINT)` 读 tag 与目标。
  **只在 `FILE_ATTRIBUTE_REPARSE_POINT` 置位时才开句柄**，不然每个文件都多一次系统调用。
- `FindFirstStreamW` / `FindNextStreamW` 枚举 ADS，`::$DATA`（默认流）不算。
- 解析逻辑（`reparse_target` / `stream_name_of`）**是平台无关的纯函数**，
  所以能在 macOS 上单测；`#[cfg]` 只剩下"去哪儿拿这坨字节"。

**验证边界（重要）**：本机是 macOS，`cargo check --target x86_64-pc-windows-msvc`
会在 `aws-lc-sys`（russh 的依赖）那里因为缺 Windows SDK 头文件挂掉，
根本到不了 core。所以改用一个只依赖 std 的探针 crate（`#[path]` 挂
`entry_kind.rs` + `i18n.rs`）对 Windows / Linux 三元做类型检查——
它可以证明"能编过"，**不能**证明"跑起来行为对"，后者仍需 Windows 真机/CI。
注意 `#[path]` 挂进来的文件不在 cargo fingerprint 里，改完源码要 `touch src/lib.rs`，
否则会看到假绿（用一次故意的类型错误验证过它确实在编译那段）。


## 7. 回收站与"放回原处"（可撤销卖点的地基）

| 平台 | 位置 | 元数据 | 放回原处的条件 |
|---|---|---|---|
| macOS | 启动卷 `~/.Trash`；其他卷 `/.Trashes/<uid>` | 启动卷：`~/.Trash/.DS_Store` 内记录（文件名 + 原始路径）；外部卷：删除时写入的出处扩展属性 | 记录在就能放回；`rm` 或第三方 API 删除的**没有记录，不能放回** |
| Windows | 每卷 `$Recycle.Bin\<SID>\` | `$I` 索引文件（v1/v2 两代格式）：version(8B) + 原始大小(8B) + 删除时间 FILETIME(8B) + 原始路径（v1 固定 520B UTF-16LE；v2 变长，天然突破 260 限制）；`$R` 文件存数据 | 必须走 Shell `FOF_ALLOWUNDO` 才会生成 `$I`/`$R`；Shift+Delete、命令行直删没有 |
| Linux | home：`$XDG_DATA_HOME/Trash`（默认 `~/.local/share/Trash`）；外部卷：挂载点 `.Trash/$uid`（sticky）或 `.Trash-$uid` | `info/*.trashinfo`：`[Trash Info]` + `Path=`（URL 编码）+ `DeletionDate=`（RFC 3339） | `.trashinfo` 丢失则无法自动恢复 |

**工程铁律：一律走平台 trash API**（macOS `NSFileManager trashItem` / Windows `SHFileOperation` 带 `FOF_ALLOWUNDO` / Linux freedesktop 规范实现，如 `trash` crate）。禁止自己把文件 `mv` 进回收站目录——否则放回原处元数据缺失，"可撤销"直接破功。附加约束：跨卷删除时回收站位于文件所在卷；文件超过回收站容量时 Windows 会询问或直接永久删除。

### 7.1 两种删除方式与可移动介质（2026-11 落地）

| 方式 | 走哪儿 | 能撤销 | 用在哪 |
|---|---|---|---|
| **移到回收站** | 平台 trash API（`trash` crate） | ✅ 能放回原处 | 内置卷 / 固定磁盘 |
| **彻底删除** | 自己递归删，不跟链接、不写回收站 | ❌ **不可撤销** | 用户显式要求；**以及可移动介质上唯一允许的方式** |

**可移动介质只允许彻底删除**，这不是省事，是因为卷上的回收站本来就靠不住：

- 位置在**那根 U 盘上**（macOS `/.Trashes/<uid>`、Windows `$Recycle.Bin`、Linux `.Trash-<uid>`），
  插到另一台机器上多数看不见，用户以为删了其实还占着空间。
- 拔盘/换机器之后"放回原处"多半失败，撤销就变成一句空话。
- U 盘容量小，回收站会白占一块。

判定用**一次系统调用**，不起 `diskutil` / 外部进程：

| 平台 | 判据 | 依据 |
|---|---|---|
| macOS | `statfs.f_flags & MNT_REMOVABLE (0x200)` | 本机 SDK `sys/mount.h`；实测 U 盘 = `1`、系统卷 = `0` |
| Linux | `/sys/dev/block/<major>:<minor>/removable == "1"` | sysfs 的块设备属性（分区继承整盘） |
| Windows | `GetDriveTypeW(盘符根) == DRIVE_REMOVABLE (2)` | Win32 `GetDriveType`，U 盘/SD 卡返回 2 |

**判不出来时按"不是可移动介质"处理**（拿不到 flags、sysfs 不存在、路径解析不出盘符）：
宁可让用户走回收站，也不要在他不知情的时候执行不可撤销的删除。这条是安全方向的默认值，
别为了"覆盖更多情况"把它翻过来。

彻底删除本身还有三条纪律：

1. **删链接不删目标**：先 `symlink_metadata` 判类型，符号链接/junction 用删文件的路径处理，
   绝不 `read_dir` 跟进去（否则删一个链接会把整棵目标树干掉）。
2. **不跟 junction/reparse**：`EntryKind::is_redirect()` 为真的项按"一项"删，不递归。
3. **只读文件先改权限再删**：Windows 上只读文件 `remove_file` 会失败，需要先
   `set_permissions(readonly = false)`；递归删除时要逐层处理。


## 8. 可移动介质与网络文件系统的坑

- **exFAT**（跨平台 U 盘事实标准）：无 xattr、无 symlink、无 POSIX 权限、无创建时间；单文件无 4GB 限制。
- **FAT32**：单文件 4GB、Windows 格式化工具 32GB 卷上限、8.3 短名兼容层。
- **SMB/NFS 挂载**：大小写行为取决于服务端与导出选项；权限映射随挂载参数变化；symlink 支持取决于配置。
- **云同步文件夹**（OneDrive Files On-Demand / iCloud 优化存储）：云端占位文件占字节为 0（reparse/eviction 状态），显示大小和执行操作前必须识别状态并触发下载。

## 9. WFiles 工程落地清单（core / fs / platform 硬约束）

1. **名称模型**：全程 `OsStr`/`PathBuf`，不假设 UTF-8；UI 显示 lossy + 提供原始字节视图。
2. **创建前校验**（按目标文件系统）：Windows 拦保留名（含扩展名、大小写、上标变体）+ 非法字符 + 结尾空格/句点；所有平台拦单组件超长（Windows 按 UTF-16 units，POSIX 按字节）+ 预估路径总长（Win 260/32767、macOS 1024、Linux 4096）。读取不校验，仅警告。
3. **冲突预检**：复制到大小写不敏感卷前检测 case 冲突与 NFC/NFD 碰撞，提前走冲突四策略（跳过/覆盖/重命名/合并）。
4. **长路径**：manifest 声明 `longPathAware`；内部统一 verbatim 表示；目标路径超 260 时 UI 显式提示（用户系统可能未开 `LongPathsEnabled`）。
5. **回收站**：只走平台 API；侧栏回收站入口展示原始位置；放回失败（原目录已删/名称被占）要有明确的人工恢复流程。
6. **元数据保留**：跨卷搬运前抽样 20 项扫 xattr/ADS/权限，把"可能不保留的类别"
   并进这次操作的结果提示；同卷不报（同卷复制不会丢，每次都报就是噪音）。
   **仍未做**：真正把 xattr/ADS 复制过去（现在只提示），以及"保留/放弃"的选择框。
7. **链接策略**：默认跟随，可配置保留；不支持的文件系统自动降级 + 提示。
8. **时间戳**：创建时间按平台可用性降级显示；同步/备份语义依赖 mtime，复制时尽力保留。
9. **隐藏文件**：三平台语义（前导点 / hidden 属性 / Finder 隐藏标志）在 platform 层统一为"隐藏"概念。
10. **云占位文件（P4）**：识别 reparse/eviction 状态，显示"在云端，需下载"，操作前触发下载。
11. **删除两种方式**：回收站（可撤销）与彻底删除（不可撤销）分开建模，别做成一个布尔开关；
    可移动介质上只允许彻底删除；彻底删除**不进撤销栈**，判"是不是可移动"失败时按固定磁盘处理。
    （判定与纪律见 7.1）

## 参考来源

- Microsoft Learn：Naming Files, Paths, and Namespaces；Maximum Path Length Limitation
- Microsoft：[MS-FSCC] 2.1.2.1 Reparse Tags；File Attribute Constants (WinNT.h)；
  Reparse Points / Reparse Point Operations；FSCTL_SET_REPARSE_POINT；FindFirstStreamW
- Microsoft：[MS-SHLLINK] 2.1 ShellLinkHeader、2.3 LinkInfo
- Apple：TN3150 Getting ready for dataless files；Synchronizing the File Provider Extension
- libyal Windows Recycle.Bin 格式规范（`trash-core` crate 文档）
- freedesktop.org Trash Specification v1.0
- Apple：APFS 卷格式文档、TN1150（HFS+ 名称约束）；The Eclectic Light Company：APFS Directories and Names
- Wikipedia：Comparison of file systems；Linux man pages（path_resolution、statx）
