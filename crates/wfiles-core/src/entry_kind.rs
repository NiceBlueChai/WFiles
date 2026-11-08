//! 文件系统特殊对象的**统一语义层**。
//!
//! # 为什么单独一层
//!
//! 符号链接、硬链接、Windows junction / reparse point、快捷方式、云占位文件
//! 分属三个不同层次（文件系统对象 / 用户态文件格式 / 云厂商私有属性），
//! 没有任何一个第三方库能统一处理。所以 core 自己定义语义，界面和搬运代码
//! 只认 [`EntryKind`]，不认 `#[cfg(target_os)]`。
//!
//! 平台相关的取值全部集中在 [`probe`] 的少数 `#[cfg]` 分支；其余判定写成
//! 纯函数 [`classify`]，输入是纯数据 [`Probe`]，因此三平台的判定规则在本机
//! 也能编译并单测。
//!
//! # 官方规格来源
//!
//! - Microsoft, *[MS-FSCC] 2.1.2.1 Reparse Tags*：
//!   `IO_REPARSE_TAG_MOUNT_POINT = 0xA0000003`、
//!   `IO_REPARSE_TAG_SYMLINK = 0xA000000C`；tag 的 bit 29 是
//!   **Name Surrogate** 位（置位表示"这一项代表系统里另一个命名实体"），
//!   <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/c8e77b37-3909-4fe6-a4ea-2b9d423b1ee4>
//! - Microsoft, *File Attribute Constants (WinNT.h)*：
//!   `FILE_ATTRIBUTE_REPARSE_POINT 0x00000400`、`OFFLINE 0x00001000`、
//!   `RECALL_ON_OPEN 0x00040000`、`PINNED 0x00080000`、
//!   `RECALL_ON_DATA_ACCESS 0x00400000`，
//!   <https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants>
//! - Microsoft, *[MS-SHLLINK] 2.1 ShellLinkHeader*：
//!   `HeaderSize MUST be 0x0000004C`，`LinkCLSID MUST be
//!   00021401-0000-0000-C000-000000000046`，
//!   <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-shllink/c3376b21-0931-45e4-b2fc-a48ac0e60d15>
//! - freedesktop.org, *Desktop Entry Specification v1.5*：
//!   `.desktop` 文件是 `[Desktop Entry]` 组的键值文本，`Type` / `Name` 必需，
//!   `Exec` / `URL` 给出目标，
//!   <https://specifications.freedesktop.org/desktop-entry-spec/latest/>
//! - Apple, *TN3150: Getting ready for dataless files*：只含元数据的文件叫
//!   **dataless**，官方判据是 `stat.st_flags & SF_DATALESS`；并明确警告
//!   `fileExists(atPath:)` / 枚举目录都可能触发 materialization（联网下载），
//!   <https://developer.apple.com/documentation/technotes/tn3150-getting-ready-for-data-less-files>
//! - Apple, *Synchronizing the File Provider Extension*：iCloud Drive 等 provider
//!   的本地副本分 dataless / materialized 两种，
//!   <https://developer.apple.com/documentation/FileProvider/synchronizing-the-file-provider-extension>
//! - Microsoft, *FSCTL_SET_REPARSE_POINT*：设置 reparse point 需要
//!   `SE_CREATE_SYMBOLIC_LINK_NAME` 权限，<https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_set_reparse_point>
//! - Microsoft, *FindFirstStreamW*：ADS 是 `::$DATA` 具名流，
//!   <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirststreamw>
//!
//! 注意 Apple 的警告对 WFiles 是硬约束：判云占位**不能**为了多看一眼内容
//! 去读文件；这里只在 `probe()` 已有的那次 `symlink_metadata` 上顺手取
//! `st_flags`，零额外 IO、零 materialization。

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::i18n::{Message, Msg};

/// Name Surrogate 位（MS-FSCC 2.1.2.1 的 N 位，bit 29）。
/// 置位的 tag 会重定向路径，遍历时**绝不能**当普通目录走进去。
const REPARSE_NAME_SURROGATE: u32 = 0x2000_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// macOS `sys/stat.h` 的 `SF_DATALESS`：APFS 上"只有元数据、内容在云上"。
/// 与 Apple TN3150 的官方判据一致；值取自本机 CommandLineTools SDK 的
/// `/usr/include/sys/stat.h`（`0x40000000`），写死以免不同 SDK 版本漂移。
const SF_DATALESS: u32 = 0x4000_0000;

/// MS-SHLLINK 的 ShellLinkHeader：HeaderSize(4) + LinkCLSID(16)。
const LNK_HEADER: [u8; 20] = [
    0x4C, 0x00, 0x00, 0x00, // HeaderSize = 0x0000004C
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, // 00021401-0000-0000-
    0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, // C000-000000000046
];

/// 快捷方式的种类。这三种都是**普通文件**，不是文件系统链接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutFlavor {
    /// Windows `.lnk`（MS-SHLLINK）。
    WindowsLnk,
    /// macOS Finder 别名 / bookmark data（魔数 `book`）。
    MacAlias,
    /// freedesktop `.desktop`。
    DesktopEntry,
}

impl ShortcutFlavor {
    /// 中文是 i18n key，界面拿去 `tr(cx, …)`。
    pub fn label(&self) -> &'static str {
        match self {
            Self::WindowsLnk => "Windows 快捷方式",
            Self::MacAlias => "Finder 别名",
            Self::DesktopEntry => "桌面快捷方式",
        }
    }
}

/// 一项到底是什么。界面、遍历、搬运、统计都只认这个枚举。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// 符号链接。`broken` 为目标不存在（仍能列出、能删，不能当目标内容用）。
    Symlink {
        target: Option<PathBuf>,
        broken: bool,
    },
    /// Windows junction / mount point：Name Surrogate 型的 reparse point。
    Junction {
        target: Option<PathBuf>,
    },
    /// 其它 reparse point（去重、WSL、ProjFS…）。语义归过滤器所有，一律不递归。
    ReparseOther {
        tag: u32,
    },
    /// 硬链接：同一 inode 的第 N 个名字。`links` 是硬链接计数。
    HardLink {
        links: u64,
    },
    /// 用户态快捷方式文件。
    Shortcut {
        flavor: ShortcutFlavor,
        target: Option<PathBuf>,
    },
    /// 云占位：数据不在本地，操作前需要先下载。
    CloudPlaceholder,
}

impl EntryKind {
    /// 会不会把路径解析重定向到别处。遍历/复制/统计遇到这些必须停。
    pub fn is_redirect(&self) -> bool {
        matches!(
            self,
            Self::Symlink { .. } | Self::Junction { .. } | Self::ReparseOther { .. }
        )
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// 界面上的类型名。中文是 key，翻译走既有 i18n 表。
    pub fn label(&self) -> &'static str {
        match self {
            Self::File => "文件",
            Self::Directory => "文件夹",
            Self::Symlink { broken: true, .. } => "失效的符号链接",
            Self::Symlink { .. } => "符号链接",
            Self::Junction { .. } => "联接点",
            Self::ReparseOther { .. } => "重解析点",
            Self::HardLink { .. } => "硬链接",
            Self::Shortcut { flavor, .. } => flavor.label(),
            Self::CloudPlaceholder => "云端文件（需下载）",
        }
    }

    /// 链接/快捷方式指向哪儿（能读出来时）。
    pub fn target(&self) -> Option<&Path> {
        match self {
            Self::Symlink { target, .. }
            | Self::Junction { target }
            | Self::Shortcut { target, .. } => target.as_deref(),
            _ => None,
        }
    }

    /// 操作前是否需要先把云端数据取回本地。
    pub fn needs_hydration(&self) -> bool {
        matches!(self, Self::CloudPlaceholder)
    }
}

/// 复制时怎么处理链接。默认跟随目标内容（与 Explorer / Finder 一致）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkPolicy {
    #[default]
    Follow,
    /// 复制链接本身；目标文件系统不支持时自动降级为 `Follow` 并回报。
    Preserve,
}

/// 搬运选项。默认值与历史上 `copy_entries` 的行为完全一致。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyOptions {
    pub link_policy: LinkPolicy,
    /// 尽力保留 xattr / 时间戳；丢之前由调用方提示用户。
    pub preserve_metadata: bool,
}

/// 一次探测拿到的**原始事实**。纯数据，不含平台句柄，所以 classify 可单测。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub is_symlink: bool,
    pub is_dir: bool,
    pub nlink: Option<u64>,
    pub windows_attributes: Option<u32>,
    /// `stat.st_flags`（macOS）。`SF_DATALESS` 置位即 dataless / 云占位。
    pub macos_flags: Option<u32>,
    /// `FSCTL_GET_REPARSE_POINT` 读到的 tag；拿不到时按 Name Surrogate 保守处理。
    pub reparse_tag: Option<u32>,
    pub link_target: Option<PathBuf>,
    pub broken: bool,
    pub name: String,
    /// 只对已知快捷方式扩展名读文件头，避免给列目录每个文件都加一次 IO。
    pub head: Vec<u8>,
}

/// 纯判定：把原始事实翻译成统一语义。三平台共用这一份规则。
pub fn classify(probe: &Probe) -> EntryKind {
    // Apple TN3150：dataless 优先于一切其它分类。文件本身可能同时是
    // reparse/硬链接，但"内容不在这儿"是搬运前最要紧的事实。
    // 读 flags 不触发 materialization，读内容才会。
    if probe.macos_flags.unwrap_or(0) & SF_DATALESS != 0 {
        return EntryKind::CloudPlaceholder;
    }
    let attrs = probe.windows_attributes.unwrap_or(0);
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;

    if reparse {
        // 先看是不是重定向类：symlink、Name Surrogate tag（junction / mount point）。
        // 微软文档明确 N 位表示"代表另一个命名实体"，不认识的 tag 也照此判断。
        if probe.is_symlink {
            return EntryKind::Symlink {
                target: probe.link_target.clone(),
                broken: probe.broken,
            };
        }
        let tag = probe.reparse_tag.unwrap_or(0);
        if tag & REPARSE_NAME_SURROGATE != 0 {
            return EntryKind::Junction {
                target: probe.link_target.clone(),
            };
        }
        // 云占位由属性位判定，不依赖 tag 数值（CLOUD_* 是一组厂商自定义 tag）。
        if attrs
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
        {
            return EntryKind::CloudPlaceholder;
        }
        // 拿不到 tag 又像目录的 reparse point（junction 在 Windows 上 is_dir 为 true）：
        // 宁可当成联接点不递归，也不能 read_dir 走进目标树。
        return if probe.is_dir {
            EntryKind::Junction {
                target: probe.link_target.clone(),
            }
        } else {
            EntryKind::ReparseOther { tag }
        };
    }

    // 非 reparse 的云占位（部分同步客户端只设属性位）。
    if attrs
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
    {
        return EntryKind::CloudPlaceholder;
    }

    // 只置 PINNED 表示"用户要留在本地"，是普通文件，不是待下载占位。
    if attrs & FILE_ATTRIBUTE_PINNED != 0 {
        return EntryKind::File;
    }

    if probe.is_symlink {
        return EntryKind::Symlink {
            target: probe.link_target.clone(),
            broken: probe.broken,
        };
    }

    if probe.is_dir {
        return EntryKind::Directory;
    }

    // 旧版 iCloud Drive（Sonoma 之前）用「.原名.icloud」的隐藏 plist 占位。
    // 它不是 dataless（本地确实有个小文件，但内容是 plist 不是原件），
    // 复制它复制的不是用户以为的那份文件。
    //
    // Apple 没有文档化这个命名，所以判得保守些：**必须是隐藏名 + `.icloud` 结尾**。
    // 用户自己放一个 `photo.icloud` 不会被误判。
    if probe.name.starts_with('.') && probe.name.ends_with(".icloud") {
        return EntryKind::CloudPlaceholder;
    }

    if let Some(flavor) = shortcut_flavor(&probe.name, &probe.head) {
        let target = match flavor {
            ShortcutFlavor::DesktopEntry => desktop_target(&probe.head),
            ShortcutFlavor::WindowsLnk => lnk_target(&probe.head),
            // Apple 从未公开 Finder 别名 / bookmark data 的二进制布局，
            // 没有官方字段可读。只认种类，不猜目标——猜错比不显示更糟。
            ShortcutFlavor::MacAlias => None,
        };
        return EntryKind::Shortcut { flavor, target };
    }

    // Unix 目录本身 nlink 也 > 1，所以这一条必须排在 is_dir 之后。
    if let Some(links) = probe.nlink
        && links > 1
    {
        return EntryKind::HardLink { links };
    }

    EntryKind::File
}

/// 遍历时判断"这是不是链接/联接点"的三平台统一入口。
/// Unix 上 `DirEntry::file_type` 已经不跟随，零额外 IO；Windows 上 junction
/// 的 `file_type` 看不出来，要补一次 `symlink_metadata` 看 REPARSE_POINT 位。
pub fn is_link_or_junction(entry: &fs::DirEntry) -> bool {
    if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt as _;
        if let Ok(meta) = fs::symlink_metadata(entry.path()) {
            return meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        }
    }
    false
}

/// 遍历/复制/统计是否应该递归进去。硬链接、正常目录可以；重定向类一律停。
pub fn should_descend(kind: &EntryKind) -> bool {
    kind.is_dir() && !kind.is_redirect()
}

/// 同一份数据的身份：Unix 上是 `(设备号, inode)`。
///
/// 查重复用它去重——同一 inode 的多个硬链接不是「重复文件」，
/// 删掉一个不释放任何空间。
///
/// Windows 上拿这个要 `GetFileInformationByHandle`（每个文件开一次句柄），
/// 对"扫几万个文件"的查重复来说太贵，这里返回 `None`：Windows 侧宁可多报，
/// 也不把两份真数据当成一份。
pub fn file_identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// 只读识别快捷方式。返回 None 表示就是普通文件。
pub fn shortcut_flavor(name: &str, head: &[u8]) -> Option<ShortcutFlavor> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".lnk") && head.starts_with(&LNK_HEADER) {
        return Some(ShortcutFlavor::WindowsLnk);
    }
    if lower.ends_with(".desktop") && head.starts_with(b"[Desktop Entry]") {
        return Some(ShortcutFlavor::DesktopEntry);
    }
    // macOS Finder 别名 / bookmark data 以 "book" 开头；没有固定扩展名，
    // 所以只在按需探测（选中某一项）时调用，不进列目录热路径。
    if head.starts_with(b"book") {
        return Some(ShortcutFlavor::MacAlias);
    }
    None
}

/// `.desktop` 里 `Exec=` / `URL=` / `Path=` 指向哪儿（freedesktop spec v1.5）。
fn desktop_target(head: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        let line = line.trim();
        for key in ["URL=", "Path=", "Exec="] {
            if let Some(value) = line.strip_prefix(key) {
                let value = value.trim();
                if !value.is_empty() {
                    // Exec 里第一个空白前是程序名，其余是参数。
                    let first = value.split_whitespace().next().unwrap_or(value);
                    return Some(PathBuf::from(first));
                }
            }
        }
    }
    None
}

/// 从 `.lnk` 里读目标路径，只认官方 [MS-SHLLINK] 2.3 `LinkInfo` 的布局：
///
/// ```text
/// LinkInfoSize(4) | LinkInfoHeaderSize(4) | LinkInfoFlags(4) |
/// VolumeIDOffset(4) | LocalBasePathOffset(4) |
/// CommonNetworkRelativeLinkOffset(4) | CommonPathSuffixOffset(4)
/// [LocalBasePathOffsetUnicode(4) | CommonPathSuffixOffsetUnicode(4)]  ← HeaderSize ≥ 0x24
/// ```
///
/// 目标 = `LocalBasePath` + `CommonPathSuffix`（两个都是 null 结尾字符串）。
/// `LinkInfo` 本身紧跟在 `LinkTargetIDList` 后面，后者的长度写在它首两个字节。
///
/// 只做"读出来显示"，**绝不**拿它去访问目标：目标可能早被删了，
/// 也可能根本在另一台机器上（UNC）。
fn lnk_target(head: &[u8]) -> Option<PathBuf> {
    const HAS_LINK_TARGET_ID_LIST: u32 = 0x1;
    const HAS_LINK_INFO: u32 = 0x2;
    const VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 0x1;

    if !head.starts_with(&LNK_HEADER) {
        return None;
    }
    let flags = u32_at(head, 0x14)?;
    if flags & HAS_LINK_INFO == 0 {
        return None;
    }
    let mut at: usize = 0x4C;
    if flags & HAS_LINK_TARGET_ID_LIST != 0 {
        at = at.checked_add(2 + u16_at(head, at)? as usize)?;
    }
    let info = head.get(at..)?;
    // 所有偏移都必须落在 LinkInfoSize 划定的范围内（规范明确要求）。
    let info = info.get(..u32_at(info, 0)? as usize)?;
    let header_size = u32_at(info, 4)? as usize;
    if header_size < 0x1C || u32_at(info, 8)? & VOLUME_ID_AND_LOCAL_BASE_PATH == 0 {
        return None;
    }
    let (base_at, suffix_at, wide) = if header_size >= 0x24 {
        (
            u32_at(info, 0x1C)? as usize,
            u32_at(info, 0x20)? as usize,
            true,
        )
    } else {
        (
            u32_at(info, 0x10)? as usize,
            u32_at(info, 0x18)? as usize,
            false,
        )
    };
    let base = c_string(info, base_at, wide).filter(|text| !text.is_empty())?;
    let suffix = c_string(info, suffix_at, wide).unwrap_or_default();
    Some(PathBuf::from(format!("{base}{suffix}")))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// 读 null 结尾的字符串。`wide` 为真按 UTF-16LE 读（Unicode 字段），
/// 否则按单字节读——ANSI 字段用 Windows 本地代码页，不查代码页表就
/// 只能按 Latin-1 近似；中文目标请以 Unicode 字段为准（有就用）。
fn c_string(bytes: &[u8], at: usize, wide: bool) -> Option<String> {
    let rest = bytes.get(at..)?;
    if wide {
        let mut units = Vec::new();
        let mut ix = 0;
        while let Some(pair) = rest.get(ix..ix + 2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
            ix += 2;
        }
        Some(String::from_utf16_lossy(&units))
    } else {
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(rest.len());
        Some(rest[..end].iter().map(|byte| *byte as char).collect())
    }
}

/// 按需探测某个文件的快捷方式类型（不在列目录热路径上）。
pub fn inspect_shortcut(path: &Path) -> Option<ShortcutFlavor> {
    let name = path.file_name()?.to_string_lossy();
    let head = read_head(path, 4096);
    shortcut_flavor(&name, &head)
}

fn read_head(path: &Path, limit: usize) -> Vec<u8> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; limit];
    let read = file.read(&mut buf).unwrap_or(0);
    buf.truncate(read);
    buf
}

/// 采集平台事实。只有这个函数的 `#[cfg]` 分支碰 OS API。
pub fn probe(entry: &fs::DirEntry) -> Probe {
    let path = entry.path();
    let name = entry.file_name().to_string_lossy().into_owned();
    let file_type = entry.file_type().ok();
    let is_symlink = file_type.is_some_and(|kind| kind.is_symlink());
    let meta = fs::symlink_metadata(&path).ok();
    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());

    let mut out = Probe {
        is_symlink,
        is_dir,
        name: name.clone(),
        ..Default::default()
    };

    if is_symlink {
        out.link_target = fs::read_link(&path).ok();
        // 断链：`fs::metadata` 跟随失败，`symlink_metadata` 仍在。
        out.broken = fs::metadata(&path).is_err();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        out.nlink = meta.as_ref().map(|m| m.nlink());
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        // 顺手在已有的那次 symlink_metadata 上取，不额外 stat、更不读内容：
        // Apple TN3150 说读内容会触发 materialization（联网下载 + 卡顿）。
        out.macos_flags = meta.as_ref().map(|m| m.st_flags());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt as _;
        out.windows_attributes = meta.as_ref().map(|m| m.file_attributes());
        // 开句柄不便宜，只有 REPARSE_POINT 位置了才去问具体 tag。
        // 拿不到（权限/驱动不认）就保持 None，`classify` 会按官方的
        // 保守规则把目录型 reparse 当联接点——宁可不递归，也不走进目标树。
        if out
            .windows_attributes
            .is_some_and(|attrs| attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            && let Some((tag, target)) = win32::reparse_info(&path)
        {
            out.reparse_tag = Some(tag);
            if out.link_target.is_none() {
                out.link_target = target;
            }
        }
    }

    // 列目录只给已知扩展名读文件头，别给每个文件都加一次 IO。
    let lower = name.to_ascii_lowercase();
    if !is_dir && (lower.ends_with(".lnk") || lower.ends_with(".desktop")) {
        out.head = read_head(&path, 4096);
    }

    out
}

/// 直接按路径探测（搬运/删除时没有 DirEntry）。
pub fn probe_path(path: &Path) -> Probe {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = fs::symlink_metadata(path).ok();
    let is_symlink = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
    let mut out = Probe {
        is_symlink,
        is_dir,
        name: name.clone(),
        ..Default::default()
    };
    if is_symlink {
        out.link_target = fs::read_link(path).ok();
        out.broken = fs::metadata(path).is_err();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        out.nlink = meta.as_ref().map(|m| m.nlink());
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        // 顺手在已有的那次 symlink_metadata 上取，不额外 stat、更不读内容：
        // Apple TN3150 说读内容会触发 materialization（联网下载 + 卡顿）。
        out.macos_flags = meta.as_ref().map(|m| m.st_flags());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt as _;
        out.windows_attributes = meta.as_ref().map(|m| m.file_attributes());
        if out
            .windows_attributes
            .is_some_and(|attrs| attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            && let Some((tag, target)) = win32::reparse_info(path)
        {
            out.reparse_tag = Some(tag);
            if out.link_target.is_none() {
                out.link_target = target;
            }
        }
    }
    let lower = name.to_ascii_lowercase();
    if !is_dir && (lower.ends_with(".lnk") || lower.ends_with(".desktop")) {
        out.head = read_head(path, 4096);
    }
    out
}

/// 按路径判"是不是重定向类"（符号链接 / 联接点 / 未知 reparse）。
///
/// 删除时用它：**重定向类只删这一项本身，绝不跟进去**。别改用
/// `symlink_metadata().is_dir()`——Windows 的 junction 在那里为 `true`，
/// 照它递归会把联接点指向的整棵树删掉，而且不可撤销。
pub fn is_redirect_path(path: &Path) -> bool {
    classify(&probe_path(path)).is_redirect()
}

/// 这一项在复制前是否需要提示"数据不在本地"。
///
/// 搬运链自己用 `classify(probe_path(..)).needs_hydration()`（它已经有
/// `EntryKind` 了，不必探两次）；这个是给界面按路径判一次用的入口。
pub fn needs_hydration(path: &Path) -> bool {
    classify(&probe_path(path)).needs_hydration()
}

/// 复制前的元数据丢失风险。纯数据，界面拿去决定要不要弹提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataRisk {
    /// 有扩展属性（Finder 标签 / quarantine / Linux user.*）。
    ExtendedAttributes { names: Vec<String> },
    /// Windows 备用数据流（Zone.Identifier 等）；跨平台不可移植。
    AlternateDataStreams,
    /// POSIX 权限 / 属主，复制到不区分权限的卷会丢。
    PosixPermissions,
    /// 创建时间目标卷不记录。
    CreationTime,
}

impl MetadataRisk {
    /// 中文是 i18n key，界面拿去 `tr(cx, …)`。
    pub fn label(&self) -> &'static str {
        match self {
            Self::ExtendedAttributes { .. } => "扩展属性",
            Self::AlternateDataStreams => "备用数据流",
            Self::PosixPermissions => "权限",
            Self::CreationTime => "创建时间",
        }
    }
}

/// 跨卷搬运前的元数据风险摘要：一条能直接上屏的话。
///
/// # 为什么只在跨卷时报
///
/// 同卷复制/移动不会丢 xattr / 权限 / 创建时间；每次都报一遍，
/// 用户很快就学会无视它——那时真丢了也没人看。
///
/// # 为什么只抽前 20 项
///
/// `metadata_risks` 在 macOS/Linux 上要起外部进程（`xattr` / `getfattr`）。
/// 复制一千个文件就起一千次进程，等于把搬运拖慢一个数量级。
/// 抽到的样本已经足够回答"这批里有没有带元数据的"这个问题。
pub fn metadata_risk_note(sources: &[PathBuf], dest_dir: &Path) -> Option<Msg> {
    const SAMPLE: usize = 20;

    if !crosses_volume(sources, dest_dir) {
        return None;
    }
    let mut risky = 0usize;
    let mut kinds: Vec<&'static str> = Vec::new();
    for source in sources.iter().take(SAMPLE) {
        let risks = metadata_risks(source);
        if risks.is_empty() {
            continue;
        }
        risky += 1;
        for risk in risks {
            let label = risk.label();
            if !kinds.contains(&label) {
                kinds.push(label);
            }
        }
    }
    if risky == 0 {
        return None;
    }
    Some(Message::args(
        "{count} 项带有目标卷可能不保留的元数据（{kinds}）",
        &[("count", risky.to_string()), ("kinds", kinds.join("、"))],
    ))
}

/// 源和目标是不是不同的卷。目标目录可能还不存在，往上找第一个存在的祖先。
fn crosses_volume(sources: &[PathBuf], dest_dir: &Path) -> bool {
    let Some(dest_dev) = ancestor_device(dest_dir) else {
        return false;
    };
    sources
        .iter()
        .any(|source| ancestor_device(source).is_some_and(|dev| dev != dest_dev))
}

#[cfg(unix)]
fn ancestor_device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    let mut current = path;
    loop {
        if let Ok(meta) = fs::metadata(current) {
            return Some(meta.dev());
        }
        current = current.parent()?;
    }
}

/// Windows 上拿卷号要 `GetVolumeInformationByHandleW`（还得先开句柄），
/// 对"每次搬运前扫一眼"来说不值得；这里直接说"判不了"，
/// 宁可少提示，也不猜。
#[cfg(not(unix))]
fn ancestor_device(_path: &Path) -> Option<u64> {
    None
}

/// 尽力扫描会丢的元数据。**只报告，不改文件**；读不到就当没有，
/// 绝不能因为探测失败就拦住用户的搬运。
///
/// 只报**用户真正在意的**那几类（见 [`matters`]）：macOS 给每个文件都塞
/// `com.apple.FinderInfo` / `com.apple.provenance`，全报出来等于没报。
///
/// 调用它的地方是 [`metadata_risk_note`]（跨卷搬运前抽 20 项扫一次）。
/// 别把它放进详情板：那是每帧重画，而这里一次要起一个外部进程。
pub fn metadata_risks(path: &Path) -> Vec<MetadataRisk> {
    let mut out = Vec::new();
    if let Some(names) = extended_attribute_names(path) {
        let names: Vec<String> = names.into_iter().filter(|name| matters(name)).collect();
        if !names.is_empty() {
            out.push(MetadataRisk::ExtendedAttributes { names });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let Ok(meta) = fs::symlink_metadata(path) {
            // 非默认权限/属主：跨卷复制时最容易悄悄丢掉。
            if meta.mode() & 0o777 != 0o644 && meta.mode() & 0o777 != 0o755 {
                out.push(MetadataRisk::PosixPermissions);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 真的去枚举具名流。`:::$DATA` 是默认流，不算 ADS。
        if !win32::named_streams(path).is_empty() {
            out.push(MetadataRisk::AlternateDataStreams);
        }
    }
    out
}

/// 这条扩展属性值不值得告诉用户。
///
/// 判据来自"用户会为它掉东西感到可惜吗"：
/// - `com.apple.quarantine`：下载来源与安全提示，丢了系统不再拦这个文件；
/// - `com.apple.metadata:_kMDItemUserTags`：Finder 标签/颜色；
/// - `com.apple.metadata:kMDItemWhereFroms`："从哪儿下载的"；
/// - `user.*`：Linux 上用户自己设的。
///
/// 系统自动加的（`com.apple.FinderInfo`、`com.apple.provenance`、
/// `com.apple.lastuseddate#PS`…）**每个文件都有**，不在此列。
fn matters(name: &str) -> bool {
    name.starts_with("user.")
        || name == "com.apple.quarantine"
        || name.starts_with("com.apple.metadata:_kMDItemUserTags")
        || name.starts_with("com.apple.metadata:kMDItemWhereFroms")
}

/// 只列扩展属性名，不读值（读值可能很大）。
#[cfg(target_os = "macos")]
fn extended_attribute_names(path: &Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("xattr")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn extended_attribute_names(path: &Path) -> Option<Vec<String>> {
    // Linux 上用 `getfattr -d` 的绝对名字；没有这个工具时静默降级。
    let output = std::process::Command::new("getfattr")
        .arg("--absolute-names")
        .arg("-d")
        .arg("-m")
        .arg("-")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("user.") || line.contains("user."))
            .map(String::from)
            .collect(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn extended_attribute_names(_path: &Path) -> Option<Vec<String>> {
    None
}

/// 从 `REPARSE_DATA_BUFFER` 的原始字节里解析出重定向目标。
///
/// MountPoint / Symlink 两种 tag 共用同一段布局：
/// `Tag(4) + DataLength(2) + Reserved(2) + SubstituteNameOffset(2) +
/// SubstituteNameLength(2) + PrintNameOffset(2) + PrintNameLength(2) + PathBuffer`。
/// 两个偏移都是**相对 `PathBuffer` 起点**的字节偏移。
///
/// 优先 `PrintName`（用户看得懂的 `C:\x`），退回 `SubstituteName`
/// （`\??\C:\x` / `\??\UNC\server\share`，要去掉 `\??\` 前缀）。
///
/// **纯函数**：喂字节就行，不需要 Windows。所以三平台的解析规则都能在本机单测，
/// `#[cfg]` 只剩下"去哪儿拿这坨字节"。
pub fn reparse_target(buffer: &[u8], tag: u32) -> Option<PathBuf> {
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
    if tag != IO_REPARSE_TAG_MOUNT_POINT && tag != IO_REPARSE_TAG_SYMLINK {
        return None;
    }
    let at = |offset: usize| -> Option<usize> {
        Some(u16::from_le_bytes(buffer.get(offset..offset + 2)?.try_into().ok()?) as usize)
    };
    let read = |offset: usize, length: usize| -> Option<String> {
        let start = 16 + offset;
        let bytes = buffer.get(start..start + length)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        Some(String::from_utf16_lossy(&units))
    };

    let print_length = at(14)?;
    if print_length > 0
        && let Some(text) = read(at(12)?, print_length)
        && !text.is_empty()
    {
        return Some(PathBuf::from(text));
    }
    let substitute_length = at(10)?;
    if substitute_length == 0 {
        return None;
    }
    let text = read(at(8)?, substitute_length)?;
    let trimmed = text.strip_prefix(r"\??\").unwrap_or(&text);
    Some(PathBuf::from(trimmed))
}

/// `WIN32_FIND_STREAM_DATA.cStreamName` → 具名流名字。
///
/// `":Zone.Identifier:$DATA"` → `Some("Zone.Identifier")`；
/// 默认流 `"::$DATA"` → `None`（默认流不是 ADS，别报成风险）。
///
/// **纯函数**，同样为了能在本机单测。
pub fn stream_name_of(stream_name: &[u16]) -> Option<String> {
    let end = stream_name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(stream_name.len());
    let text = String::from_utf16_lossy(&stream_name[..end]);
    let stream = text.strip_prefix(':')?.strip_suffix(":$DATA")?;
    (!stream.is_empty()).then(|| stream.to_string())
}

/// Windows 上真正要调系统的那几件事：读 reparse tag / 联接点目标、枚举 ADS。
///
/// 手写 `extern "system"` 而不是拉 `windows-sys`：这里只用 kernel32 的四个
/// 函数 + 两个结构体，多一个依赖换不来什么，却会把 core 的依赖表扩大一截。
/// 常量与结构体布局全部照官方文档抄（见模块头部的来源列表）。
///
/// **验证方式**：本机是 macOS，编不到这一段。用一个只依赖 std 的探针 crate
/// （`#[path]` 挂进本文件）跑 `cargo check --target x86_64-pc-windows-msvc`
/// 做类型检查；运行时行为仍要 Windows 真机/CI 才能验。
#[cfg(target_os = "windows")]
mod win32 {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    /// `HANDLE` 是 `void*`，用 isize 表示足够。
    type Handle = isize;
    const INVALID_HANDLE_VALUE: Handle = -1;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const OPEN_EXISTING: u32 = 3;
    /// 打开目录必须带这个，否则 CreateFileW 对目录直接失败。
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// 拿到的是链接/联接点本身，而不是它指向的目标。
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    /// CTL_CODE(FILE_DEVICE_FILE_SYSTEM=0x9, 42, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)
    const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    const FIND_STREAM_INFO_STANDARD: u32 = 0;

    /// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`：规范说 reparse data 不超过 16 KB。
    const MAX_REPARSE_BUFFER: usize = 16 * 1024;
    /// `WIN32_FIND_STREAM_DATA.cStreamName` 的长度：`MAX_PATH + 36`。
    const MAX_STREAM_NAME: usize = 260 + 36;

    #[repr(C)]
    struct Win32FindStreamData {
        stream_size: i64,
        stream_name: [u16; MAX_STREAM_NAME],
    }

    unsafe extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            device: Handle,
            io_control_code: u32,
            in_buffer: *mut c_void,
            in_buffer_size: u32,
            out_buffer: *mut c_void,
            out_buffer_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(object: Handle) -> i32;
        fn FindFirstStreamW(
            file_name: *const u16,
            info_level: u32,
            find_stream_data: *mut Win32FindStreamData,
            flags: u32,
        ) -> Handle;
        fn FindNextStreamW(handle: Handle, find_stream_data: *mut Win32FindStreamData) -> i32;
        fn FindClose(handle: Handle) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// 读 reparse point 的 tag 与目标（能读到时）。
    ///
    /// 目标优先取 `PrintName`（用户看得懂的 `C:\x`），退回 `SubstituteName`
    /// （`\??\C:\x`，要把 `\??\` 前缀去掉）。
    pub(super) fn reparse_info(path: &Path) -> Option<(u32, Option<PathBuf>)> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                0, // 只查属性，不要读写权限
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut buffer = vec![0u8; MAX_REPARSE_BUFFER];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_REPARSE_POINT,
                std::ptr::null_mut(),
                0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        let tag = u32::from_le_bytes(buffer.get(0..4)?.try_into().ok()?);
        Some((tag, super::reparse_target(&buffer, tag)))
    }

    /// 枚举具名数据流（ADS）。默认流 `::$DATA` 不算。
    pub(super) fn named_streams(path: &Path) -> Vec<String> {
        let name = wide(path);
        let mut data = Win32FindStreamData {
            stream_size: 0,
            stream_name: [0; MAX_STREAM_NAME],
        };
        let mut out = Vec::new();
        unsafe {
            let handle = FindFirstStreamW(name.as_ptr(), FIND_STREAM_INFO_STANDARD, &mut data, 0);
            if handle == INVALID_HANDLE_VALUE {
                return out;
            }
            loop {
                if let Some(stream) = super::stream_name_of(&data.stream_name) {
                    out.push(stream);
                }
                if FindNextStreamW(handle, &mut data) == 0 {
                    break;
                }
            }
            FindClose(handle);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_name_surrogate_tags_are_redirects_not_directories() {
        // MS-FSCC: MOUNT_POINT 0xA0000003 与 SYMLINK 0xA000000C 都带 N 位。
        for tag in [0xA000_0003, 0xA000_000C, 0xA000_001D] {
            let kind = classify(&Probe {
                windows_attributes: Some(FILE_ATTRIBUTE_REPARSE_POINT),
                reparse_tag: Some(tag),
                is_dir: true,
                ..Default::default()
            });
            assert!(kind.is_redirect(), "tag {tag:#x} 该被当成重定向");
            assert!(!should_descend(&kind), "tag {tag:#x} 绝不能递归进去");
        }
    }

    /// 按 `REPARSE_DATA_BUFFER` 的布局拼一个 MountPoint：PathBuffer 里先放
    /// SubstituteName（`\??\C:\target`）再放 PrintName（`C:\target`）。
    fn synthetic_mount_point() -> Vec<u8> {
        let substitute: Vec<u16> = r"\??\C:\target".encode_utf16().collect();
        let print: Vec<u16> = r"C:\target".encode_utf16().collect();
        let mut buffer = vec![0u8; 16 + (substitute.len() + print.len()) * 2];
        buffer[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        let data_len = (substitute.len() + print.len()) * 2;
        buffer[4..6].copy_from_slice(&(data_len as u16).to_le_bytes());
        buffer[8..10].copy_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
        buffer[10..12].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        buffer[12..14].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        buffer[14..16].copy_from_slice(&((print.len() * 2) as u16).to_le_bytes());
        let mut at = 16;
        for unit in substitute.iter().chain(print.iter()) {
            buffer[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            at += 2;
        }
        buffer
    }

    #[test]
    fn a_mount_point_target_is_read_from_the_official_buffer_layout() {
        let buffer = synthetic_mount_point();
        assert_eq!(
            reparse_target(&buffer, 0xA000_0003),
            Some(PathBuf::from(r"C:\target")),
            "优先 PrintName"
        );
        // 不是 MountPoint / Symlink 的 tag 没有这组偏移，绝不能瞎读。
        assert_eq!(reparse_target(&buffer, 0x8000_001A), None);
    }

    #[test]
    fn a_substitute_name_loses_its_device_prefix() {
        // PrintName 为空（有些工具只写 SubstituteName）时退回 Sub，
        // 并且要把 `\??\` 去掉，否则界面上会显示成 `\??\C:\target`。
        let mut buffer = synthetic_mount_point();
        buffer[14..16].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            reparse_target(&buffer, 0xA000_0003),
            Some(PathBuf::from(r"C:\target"))
        );
        // 两个名字都没有：读不出就老实返回 None。
        buffer[10..12].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(reparse_target(&buffer, 0xA000_0003), None);
    }

    #[test]
    fn stream_names_tell_ads_from_the_default_stream() {
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
        assert_eq!(
            stream_name_of(&wide(":Zone.Identifier:$DATA")),
            Some("Zone.Identifier".to_string())
        );
        // 默认流不是 ADS：判成风险会让每个从网上下载的文件都报一条。
        assert_eq!(stream_name_of(&wide("::$DATA")), None);
        // 坏数据不能 panic（缓冲区可能没有结尾 0）。
        assert_eq!(stream_name_of(&wide(":")), None);
        assert_eq!(stream_name_of(&[]), None);
    }

    #[test]
    fn only_the_metadata_users_care_about_is_reported() {
        // 丢了会让用户觉得"东西没了"的那几类
        assert!(matters("com.apple.quarantine"));
        assert!(matters("com.apple.metadata:_kMDItemUserTags"));
        assert!(matters("com.apple.metadata:kMDItemWhereFroms"));
        assert!(matters("user.comment"));
        // 系统给每个文件都塞的：报出来等于没报（狼来了）
        assert!(!matters("com.apple.FinderInfo"));
        assert!(!matters("com.apple.provenance"));
        assert!(!matters("com.apple.lastuseddate#PS"));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_destination_borrows_its_ancestors_volume() {
        // 目标目录还不存在时（复制到一个新建的目录名），必须借祖先的卷号；
        // 直接返回 None 会让"跨卷"永远判成 false，那条提示一次都不会出现。
        let existing = std::env::temp_dir();
        let missing = existing.join("wfiles-no-such-dir").join("deeper");
        assert_eq!(ancestor_device(&missing), ancestor_device(&existing));
    }

    #[test]
    fn an_unknown_reparse_directory_is_never_descended() {
        // 拿不到 tag 时按官方保守规则处理：宁可不递归，也不能走进目标树。
        let kind = classify(&Probe {
            windows_attributes: Some(FILE_ATTRIBUTE_REPARSE_POINT),
            is_dir: true,
            ..Default::default()
        });
        assert_eq!(kind, EntryKind::Junction { target: None });
        assert!(!should_descend(&kind));
    }

    #[test]
    fn cloud_placeholder_is_not_a_link_and_needs_hydration() {
        let kind = classify(&Probe {
            windows_attributes: Some(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_RECALL_ON_OPEN),
            reparse_tag: Some(0x9000_101A),
            ..Default::default()
        });
        assert_eq!(kind, EntryKind::CloudPlaceholder);
        assert!(kind.needs_hydration());
        assert!(!kind.is_redirect());
    }

    #[test]
    fn old_icloud_stub_files_are_placeholders_too() {
        // 旧版 iCloud Drive 的隐藏 plist 占位：`.报告.pdf.icloud`。
        assert_eq!(
            classify(&Probe {
                name: ".报告.pdf.icloud".into(),
                ..Default::default()
            }),
            EntryKind::CloudPlaceholder
        );
        // 判据故意收得紧：不在隐藏位就不当占位，免得误伤用户自己的文件。
        assert_eq!(
            classify(&Probe {
                name: "photo.icloud".into(),
                ..Default::default()
            }),
            EntryKind::File
        );
    }

    #[test]
    fn pinned_cloud_files_are_ordinary_files() {
        // PINNED 表示用户要留在本地，不是"需要下载"。
        let kind = classify(&Probe {
            windows_attributes: Some(FILE_ATTRIBUTE_PINNED),
            ..Default::default()
        });
        assert_eq!(kind, EntryKind::File);
    }

    #[test]
    fn macos_dataless_files_need_hydration_too() {
        // Apple TN3150，SF_DATALESS = 0x40000000（本机 SDK sys/stat.h）。
        // 这条不依赖 Windows 属性位，所以 macOS 上真能生效。
        let kind = classify(&Probe {
            macos_flags: Some(SF_DATALESS),
            ..Default::default()
        });
        assert_eq!(kind, EntryKind::CloudPlaceholder);
        assert!(kind.needs_hydration());
        // 置了别的 flag 但不置 SF_DATALESS，不能被误判成云占位。
        assert_eq!(
            classify(&Probe {
                macos_flags: Some(0x0008_0000),
                ..Default::default()
            }),
            EntryKind::File
        );
    }

    #[test]
    fn dataless_wins_over_every_other_classification() {
        // 同时是符号链接 / 硬链接的 dataless 项：先按"内容不在本地"处理，
        // 否则 copy_entry 会去打开它、把整份云文件下载下来。
        let kind = classify(&Probe {
            macos_flags: Some(SF_DATALESS),
            is_symlink: true,
            nlink: Some(2),
            ..Default::default()
        });
        assert_eq!(kind, EntryKind::CloudPlaceholder);
    }

    #[test]
    fn broken_symlink_stays_visible_and_is_a_redirect() {
        let kind = classify(&Probe {
            is_symlink: true,
            broken: true,
            link_target: Some(PathBuf::from("/nowhere")),
            ..Default::default()
        });
        assert!(matches!(kind, EntryKind::Symlink { broken: true, .. }));
        assert_eq!(kind.label(), "失效的符号链接");
        assert_eq!(kind.target(), Some(Path::new("/nowhere")));
    }

    #[test]
    fn hard_links_are_counted_but_directories_are_not() {
        assert_eq!(
            classify(&Probe {
                nlink: Some(3),
                ..Default::default()
            }),
            EntryKind::HardLink { links: 3 }
        );
        assert_eq!(
            classify(&Probe {
                nlink: Some(2),
                is_dir: true,
                ..Default::default()
            }),
            EntryKind::Directory
        );
    }

    #[test]
    fn shortcut_flavors_follow_the_published_headers() {
        assert_eq!(
            shortcut_flavor("app.lnk", &LNK_HEADER),
            Some(ShortcutFlavor::WindowsLnk)
        );
        // 扩展名对但头不对，不能误判成快捷方式。
        assert_eq!(shortcut_flavor("fake.lnk", b"not a link"), None);
        assert_eq!(
            shortcut_flavor(
                "firefox.desktop",
                b"[Desktop Entry]\nType=Application\nExec=/usr/bin/firefox\n"
            ),
            Some(ShortcutFlavor::DesktopEntry)
        );
        assert_eq!(
            shortcut_flavor("alias", b"book\0\0\0\0mark"),
            Some(ShortcutFlavor::MacAlias)
        );
    }

    #[test]
    fn desktop_entry_target_is_the_first_exec_token() {
        let head = b"[Desktop Entry]\nType=Application\nExec=/usr/bin/firefox --new-window\n";
        let kind = classify(&Probe {
            name: "firefox.desktop".into(),
            head: head.to_vec(),
            ..Default::default()
        });
        assert_eq!(kind.target(), Some(Path::new("/usr/bin/firefox")));
    }

    /// 按 [MS-SHLLINK] 3.1 的布局拼一个最小 `.lnk`：没有 TargetIDList，
    /// LinkInfo 直接跟在 76 字节头后面，目标是 `C:\test\a.txt`。
    fn synthetic_lnk() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x4C];
        bytes[..LNK_HEADER.len()].copy_from_slice(&LNK_HEADER);
        // LinkFlags：只置 HasLinkInfo（bit 1）。
        bytes[0x14..0x18].copy_from_slice(&2u32.to_le_bytes());

        let base = b"C:\\test\\a.txt\0";
        let suffix = b"\0";
        let info_size = 0x1C + base.len() + suffix.len();
        let mut info = vec![0u8; info_size];
        info[0x00..0x04].copy_from_slice(&(info_size as u32).to_le_bytes());
        info[0x04..0x08].copy_from_slice(&0x1Cu32.to_le_bytes()); // LinkInfoHeaderSize
        info[0x08..0x0C].copy_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
        info[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes()); // VolumeIDOffset
        info[0x10..0x14].copy_from_slice(&0x1Cu32.to_le_bytes()); // LocalBasePathOffset
        info[0x14..0x18].copy_from_slice(&0u32.to_le_bytes()); // CommonNetworkRelativeLink
        info[0x18..0x1C].copy_from_slice(&((0x1C + base.len()) as u32).to_le_bytes());
        info[0x1C..0x1C + base.len()].copy_from_slice(base);
        info[0x1C + base.len()..].copy_from_slice(suffix);
        bytes.extend_from_slice(&info);
        bytes
    }

    #[test]
    fn lnk_target_follows_the_published_link_info_layout() {
        assert_eq!(
            lnk_target(&synthetic_lnk()),
            Some(PathBuf::from("C:\\test\\a.txt"))
        );
        // 只有头、没有 LinkInfo：不能瞎读，也不能 panic。
        assert_eq!(lnk_target(&LNK_HEADER), None);
        // 判据自检：把 HasLinkInfo 关掉，同一份字节必须读不出目标。
        // 少了这条，「按规范解析成功」可能只是「恰好读到了几个字节」。
        let mut without = synthetic_lnk();
        without[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(lnk_target(&without), None);
    }

    #[test]
    fn the_scanner_is_alive() {
        // 判据自检：喂一段明确违规的数据，扫描器必须报出来。
        // 少了这条，「没扫到风险」和「本来就没风险」长得一模一样。
        let probe = Probe {
            windows_attributes: Some(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_RECALL_ON_OPEN),
            ..Default::default()
        };
        assert!(classify(&probe).needs_hydration());
        assert!(!classify(&Probe::default()).needs_hydration());
    }
}
