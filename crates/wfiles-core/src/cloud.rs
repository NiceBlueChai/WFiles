//! 云盘同步状态：本机上哪些目录在云盘的同步文件夹里，以及某个文件
//! 此刻是"本地已经有一份"还是"只在云端"。
//!
//! 只**看**，不改："始终保留在这台机器上"这类开关交给各家自己的客户端
//! 或系统设置——第三方去改占位符属性只会把同步弄乱。
//!
//! 为什么不给列表里每一行都挂角标：判状态要问文件系统（Windows 取文件
//! 属性、macOS 认 `.icloud` 占位符），一次一个 syscall。十万行的列表不能
//! 每行问一遍，所以只对**当前选中那一项**判，其余靠"这个目录在不在云盘里"
//! 这条只比路径前缀的判断——那个不用碰磁盘。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 一家云盘。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    ICloud,
    OneDrive,
    Dropbox,
    GoogleDrive,
    Nutstore,
    BaiduNetdisk,
    AliyunDrive,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::ICloud => "iCloud Drive",
            Self::OneDrive => "OneDrive",
            Self::Dropbox => "Dropbox",
            Self::GoogleDrive => "Google Drive",
            Self::Nutstore => "坚果云",
            Self::BaiduNetdisk => "百度网盘",
            Self::AliyunDrive => "阿里云盘",
        }
    }

    /// 同步文件夹常叫的名字：个人文件夹底下就找这些。
    fn folders(self) -> &'static [&'static str] {
        match self {
            Self::ICloud => &["iCloud Drive", "iCloudDrive", "iCloud"],
            Self::OneDrive => &["OneDrive"],
            Self::Dropbox => &["Dropbox"],
            Self::GoogleDrive => &["Google Drive", "GoogleDrive", "GoogleDriveFS"],
            Self::Nutstore => &["坚果云", "Nutstore"],
            Self::BaiduNetdisk => &["百度网盘同步盘", "百度网盘", "BaiduNetdiskWorkspace"],
            Self::AliyunDrive => &["阿里云盘", "AliyunDrive", "Aliyunpan"],
        }
    }

    /// macOS 把各家云盘收在 `~/Library/CloudStorage/<名字>-<账号>` 底下，
    /// 一个账号一个目录，所以只能按前缀认。
    fn storage_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::OneDrive => &["OneDrive-"],
            Self::Dropbox => &["Dropbox-"],
            Self::GoogleDrive => &["GoogleDrive-"],
            // iCloud 不在 CloudStorage 里，其余几家在 macOS 上没有这种目录。
            _ => &[],
        }
    }

    /// 个人文件夹底下这个名字算不算这一家的同步目录。
    ///
    /// 企业版 OneDrive 的目录叫 `OneDrive - 公司名`，所以除全等之外
    /// 还认"名字 + 分隔符"的前缀——免得把 `OneDrive 备份` 也当成同步目录。
    fn owns(self, folder: &str) -> bool {
        if self.folders().contains(&folder) {
            return true;
        }
        self.folders()
            .iter()
            .any(|name| folder.starts_with(name) && folder[name.len()..].starts_with(" - "))
    }
}

const ALL: [Provider; 7] = [
    Provider::ICloud,
    Provider::OneDrive,
    Provider::Dropbox,
    Provider::GoogleDrive,
    Provider::Nutstore,
    Provider::BaiduNetdisk,
    Provider::AliyunDrive,
];

/// 本机上一个云盘同步目录。
#[derive(Clone, Debug)]
pub struct SyncRoot {
    pub provider: Provider,
    pub path: PathBuf,
}

impl SyncRoot {
    /// 界面上写什么：文件夹自己的名字（`OneDrive - Contoso`），
    /// 那才是用户认得的那一个，比干巴巴的"OneDrive"有用。
    pub fn label(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.provider.name().to_string())
    }
}

/// 一个文件在云盘里的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 在同步目录里，本地有一份。
    Synced,
    /// 只有云端有：本地是个占位符，打开才会下载。
    CloudOnly,
    /// 被标成"始终保留在这台机器上"：断网也打得开。
    ///
    /// 只有 Windows 会报这一档——它把各家云盘的占位符属性统一摆在文件属性里；
    /// macOS 与 Linux 上各家都不往外暴露这个标记，所以那一档整个不编译进去。
    #[cfg(any(windows, test))]
    Pinned,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Synced => "已同步到本地",
            Self::CloudOnly => "只在云端（打开才下载）",
            #[cfg(any(windows, test))]
            Self::Pinned => "始终保留在这台机器上",
        }
    }
}

/// 测试里指定"本机上有哪些云盘"：真机器上装没装客户端由不得测试决定。
#[cfg(any(test, feature = "test-support"))]
static OVERRIDE: OnceLock<Vec<SyncRoot>> = OnceLock::new();

/// 本机上找得到的云盘同步目录。结果只算一次，之后一直用。
pub fn detected() -> &'static [SyncRoot] {
    static ROOTS: OnceLock<Vec<SyncRoot>> = OnceLock::new();
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Some(roots) = OVERRIDE.get() {
            return roots;
        }
    }
    ROOTS.get_or_init(|| {
        let home = dirs::home_dir().unwrap_or_default();
        // Windows 上 OneDrive 的路径写在环境变量里（个人版、商业版各一个），
        // 它可能被挪到 D 盘，光扫个人文件夹是找不到的。
        let mut declared: Vec<PathBuf> = Vec::new();
        for key in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
            if let Some(value) = std::env::var_os(key) {
                declared.push(PathBuf::from(value));
            }
        }
        declared.extend(dropbox_roots(&home));
        let storage = cloud_storage_dir(&home);
        roots_in(&home, storage.as_deref(), &declared)
    })
}

/// 测试里指定"本机上有哪些云盘"。
///
/// 只能装一次、之后所有测试都看这一份：它是进程级的，
/// 所以别在两条测试里各装一份互相打架的目录。
#[cfg(any(test, feature = "test-support"))]
pub fn install(roots: Vec<SyncRoot>) {
    OVERRIDE.set(roots).ok();
}

/// macOS 收各家云盘的那个目录；别的平台没有这回事。
fn cloud_storage_dir(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library").join("CloudStorage"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        None
    }
}

/// 把"个人文件夹 + 云盘收纳目录 + 环境变量给的路径"折成一份同步目录清单。
///
/// 参数都从外面喂进来：测试拿临时目录当个人文件夹就能验，不用碰真机器。
pub fn roots_in(home: &Path, storage: Option<&Path>, declared: &[PathBuf]) -> Vec<SyncRoot> {
    let mut found: Vec<SyncRoot> = Vec::new();

    for provider in ALL {
        for folder in provider.folders() {
            let path = home.join(folder);
            if path.is_dir() {
                found.push(SyncRoot { provider, path });
            }
        }
    }
    // 企业版/多账号：目录名是"名字 - 账号"，得扫一遍才认得出来。
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|it| it.to_str()) else {
                continue;
            };
            if let Some(provider) = ALL.iter().find(|it| it.owns(name)) {
                found.push(SyncRoot {
                    provider: *provider,
                    path,
                });
            }
        }
    }
    // iCloud 在 macOS 上不住在个人文件夹底下，是 Mobile Documents 里那一个。
    #[cfg(target_os = "macos")]
    {
        let icloud = home
            .join("Library")
            .join("Mobile Documents")
            .join("com~apple~CloudDocs");
        if icloud.is_dir() {
            found.push(SyncRoot {
                provider: Provider::ICloud,
                path: icloud,
            });
        }
    }
    if let Some(storage) = storage
        && let Ok(entries) = std::fs::read_dir(storage)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|it| it.to_str()) else {
                continue;
            };
            if let Some(provider) = ALL.iter().find(|it| {
                it.storage_prefixes()
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            }) {
                found.push(SyncRoot {
                    provider: *provider,
                    path,
                });
            }
        }
    }
    for path in declared {
        if !path.is_dir() {
            continue;
        }
        let provider = guess_provider(path).unwrap_or(Provider::OneDrive);
        found.push(SyncRoot {
            provider,
            path: path.clone(),
        });
    }

    // 同一条路可能被两条规矩都认出来（比如 `~/Dropbox` 既是文件夹名、
    // 又是 info.json 里写的那个）；去掉重复的，再按路径排好，
    // 免得每次启动侧栏顺序都不一样。
    found.sort_by(|left, right| left.path.cmp(&right.path));
    found.dedup_by(|left, right| left.path == right.path);
    found
}

/// 光看一条路径，猜它是哪一家的：认不出就交给调用方兜底。
fn guess_provider(path: &Path) -> Option<Provider> {
    let name = path.file_name().and_then(|it| it.to_str())?;
    ALL.iter().find(|it| it.owns(name)).copied().or_else(|| {
        ALL.iter()
            .find(|it| {
                it.storage_prefixes()
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            })
            .copied()
    })
}

/// Dropbox 的同步目录：它会被挪到别处，所以别只认 `~/Dropbox`。
/// 客户端把真实路径写在 `info.json` 里（个人版与商业版两处）。
fn dropbox_roots(home: &Path) -> Vec<PathBuf> {
    let mut files = vec![home.join(".dropbox").join("info.json")];
    if let Some(app_data) = std::env::var_os("APPDATA") {
        files.push(PathBuf::from(app_data).join("Dropbox").join("info.json"));
    }
    files
        .iter()
        .flat_map(|file| dropbox_from_info(file))
        .collect()
}

/// 从一份 `info.json` 里读出所有同步目录（个人版 + 商业版）。
fn dropbox_from_info(file: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    // 别靠 map 的遍历顺序：serde_json 开没开 `preserve_order` 由整个依赖图
    // 决定（拆成独立 crate 之后单独测 core，gpui-kit 那条链就不在了），
    // 一变顺序结果就反。info.json 认的就是这两个键，个人版在前。
    ["personal", "business"]
        .iter()
        .filter_map(|key| {
            map.get(*key)
                .and_then(|entry| entry.get("path"))
                .and_then(|it| it.as_str())
        })
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 这个路径落在哪个同步目录里。嵌套时取**最深**的那一个：
/// 把 Dropbox 放进 iCloud 里同步是常见的翻车现场，报外面那家没用。
pub fn root_for<'a>(path: &Path, roots: &'a [SyncRoot]) -> Option<&'a SyncRoot> {
    roots
        .iter()
        .filter(|root| path.starts_with(&root.path))
        .max_by_key(|root| root.path.as_os_str().len())
}

/// 这个文件在云盘里的状态；不在任何同步目录里就是 `None`。
///
/// 在同步目录里、但平台没给出"这是个占位符"的信号时，按"已同步"算：
/// 那个目录里的东西本来就是两边都有的。
/// 一次把"哪一家的"和"什么状态"都问出来：界面上一行要用两样，
/// 分开调等于把路径前缀比两遍。不在任何同步目录里就是 `None`。
///
/// 在同步目录里、但平台没给出"这是个占位符"的信号时，按"已同步"算：
/// 那个目录里的东西本来就是两边都有的。
pub fn describe<'a>(path: &Path, roots: &'a [SyncRoot]) -> Option<(&'a SyncRoot, State)> {
    let root = root_for(path, roots)?;
    let state = platform_state(path).unwrap_or(State::Synced);
    Some((root, state))
}

// ---- 平台上的"是不是占位符" ----
//
// Windows 写在文件属性里，macOS 靠 `.名字.icloud` 这个占位文件，
// Linux 上各家都不暴露这一层，所以只能认"在同步目录里"，判不出云端与否。

#[cfg(windows)]
fn platform_state(path: &Path) -> Option<State> {
    use std::os::windows::fs::MetadataExt as _;
    let attributes = std::fs::metadata(path).ok()?.file_attributes();
    state_from_attributes(attributes)
}

#[cfg(target_os = "macos")]
fn platform_state(path: &Path) -> Option<State> {
    let name = path.file_name()?.to_string_lossy();
    icloud_placeholder(&name).then_some(State::CloudOnly)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_state(_path: &Path) -> Option<State> {
    None
}

/// Windows 的占位符属性。取值来源：`std::fs::Metadata::file_attributes()`。
///
/// 认这三件事就够了：`OFFLINE` / `RECALL_*` 是"本地没有内容、要打开才下载"，
/// `PINNED` 是"用户说了永远留一份"。至于 `UNPINNED`（允许系统回收空间），
/// 文件当下还在盘上，按"已同步"显示才不吓人。
#[cfg(any(windows, test))]
pub fn state_from_attributes(attributes: u32) -> Option<State> {
    const OFFLINE: u32 = 0x0000_1000;
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const PINNED: u32 = 0x0008_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    if attributes & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        Some(State::CloudOnly)
    } else if attributes & PINNED != 0 {
        Some(State::Pinned)
    } else {
        None
    }
}

/// macOS 上"还没下载"的 iCloud 文件在盘上长这样：`.报告.txt.icloud`。
#[cfg(any(target_os = "macos", test))]
pub fn icloud_placeholder(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".icloud") && name.len() > ".icloud".len() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(root: &Path, names: &[&str]) {
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
    }

    #[test]
    fn known_folder_names_are_recognized() {
        let home = tempfile::tempdir().unwrap();
        make(
            home.path(),
            &["Dropbox", "坚果云", "百度网盘同步盘", "杂物"],
        );
        let roots = roots_in(home.path(), None, &[]);
        let providers: Vec<Provider> = roots.iter().map(|it| it.provider).collect();
        assert!(providers.contains(&Provider::Dropbox), "{providers:?}");
        assert!(providers.contains(&Provider::Nutstore), "{providers:?}");
        assert!(providers.contains(&Provider::BaiduNetdisk), "{providers:?}");
        // 不叫这些名字的目录不该被当成云盘。
        assert_eq!(roots.len(), 3, "{roots:?}");
    }

    #[test]
    fn a_business_folder_counts_but_a_lookalike_does_not() {
        let home = tempfile::tempdir().unwrap();
        make(
            home.path(),
            &["OneDrive - Contoso", "OneDrive 备份", "Google Drive"],
        );
        let roots = roots_in(home.path(), None, &[]);
        let labels: Vec<String> = roots.iter().map(|it| it.label()).collect();
        assert!(
            labels.contains(&"OneDrive - Contoso".to_string()),
            "{labels:?}"
        );
        // `OneDrive 备份` 少了那个分隔符：那是用户自己建的目录，不是同步目录。
        assert!(!labels.contains(&"OneDrive 备份".to_string()), "{labels:?}");
        assert!(labels.contains(&"Google Drive".to_string()), "{labels:?}");
    }

    #[test]
    fn macos_style_storage_folders_are_recognized() {
        let home = tempfile::tempdir().unwrap();
        let storage = home.path().join("CloudStorage");
        // 造目录时不能靠平台分支：这条测试三个平台都要跑。
        make(&storage, &["Dropbox-个人", "GoogleDrive-me", "乱放的东西"]);
        let roots = roots_in(home.path(), Some(&storage), &[]);
        let labels: Vec<String> = roots.iter().map(|it| it.label()).collect();
        assert!(labels.contains(&"Dropbox-个人".to_string()), "{labels:?}");
        assert!(labels.contains(&"GoogleDrive-me".to_string()), "{labels:?}");
        assert!(!labels.contains(&"乱放的东西".to_string()), "{labels:?}");
    }

    #[test]
    fn declared_paths_are_used_as_they_are() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        make(elsewhere.path(), &["OneDrive - Contoso"]);
        let declared = vec![elsewhere.path().join("OneDrive - Contoso")];
        let roots = roots_in(home.path(), None, &declared);
        assert_eq!(roots.len(), 1, "{roots:?}");
        assert_eq!(roots[0].provider, Provider::OneDrive);
        assert_eq!(roots[0].path, declared[0]);
    }

    #[test]
    fn the_same_folder_found_twice_is_listed_once() {
        let home = tempfile::tempdir().unwrap();
        make(home.path(), &["Dropbox"]);
        let declared = vec![home.path().join("Dropbox")];
        let roots = roots_in(home.path(), None, &declared);
        assert_eq!(roots.len(), 1, "同一条路不该列两遍：{roots:?}");
    }

    #[test]
    fn the_deepest_root_wins() {
        let home = tempfile::tempdir().unwrap();
        let outer = home.path().join("iCloudDrive");
        let inner = outer.join("Dropbox");
        std::fs::create_dir_all(&inner).unwrap();
        let roots = vec![
            SyncRoot {
                provider: Provider::ICloud,
                path: outer.clone(),
            },
            SyncRoot {
                provider: Provider::Dropbox,
                path: inner.clone(),
            },
        ];
        let found = root_for(&inner.join("notes.txt"), &roots);
        assert_eq!(found.map(|it| it.provider), Some(Provider::Dropbox));
        // 外层目录里的东西仍归外层。
        let found = root_for(&outer.join("notes.txt"), &roots);
        assert_eq!(found.map(|it| it.provider), Some(Provider::ICloud));
    }

    #[test]
    fn a_file_outside_every_root_has_no_state() {
        let home = tempfile::tempdir().unwrap();
        make(home.path(), &["Dropbox"]);
        let roots = roots_in(home.path(), None, &[]);
        assert!(describe(&home.path().join("别处").join("a.txt"), &roots).is_none());
    }

    #[test]
    fn windows_placeholder_bits_mean_cloud_only() {
        const OFFLINE: u32 = 0x0000_1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const PINNED: u32 = 0x0008_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

        assert_eq!(state_from_attributes(OFFLINE), Some(State::CloudOnly));
        assert_eq!(
            state_from_attributes(RECALL_ON_OPEN),
            Some(State::CloudOnly)
        );
        assert_eq!(
            state_from_attributes(RECALL_ON_DATA_ACCESS),
            Some(State::CloudOnly)
        );
        assert_eq!(state_from_attributes(PINNED), Some(State::Pinned));
        // UNPINNED（0x100000）只是"允许回收"，文件还在盘上。
        assert_eq!(state_from_attributes(0x0010_0000), None);
        assert_eq!(state_from_attributes(0x20), None, "普通文件不该有云盘状态");
        // 又标了"始终保留"又还没下载：以"还没下载"为准，那是用户看得见的现实。
        assert_eq!(
            state_from_attributes(PINNED | RECALL_ON_OPEN),
            Some(State::CloudOnly)
        );
    }

    #[test]
    fn an_icloud_placeholder_file_is_recognized() {
        assert!(icloud_placeholder(".报告.txt.icloud"));
        assert!(!icloud_placeholder("报告.txt"));
        assert!(!icloud_placeholder(".icloud"), "光有个后缀不算占位符");
        assert!(!icloud_placeholder("报告.txt.icloud"), "少了前面那个点不算");
    }

    #[test]
    fn dropbox_paths_come_from_its_info_file() {
        let home = tempfile::tempdir().unwrap();
        let dropbox = home.path().join(".dropbox");
        std::fs::create_dir_all(&dropbox).unwrap();
        std::fs::write(
            dropbox.join("info.json"),
            r#"{"personal": {"path": "/Users/me/Dropbox"}, "business": {"path": "/Users/me/Contoso"}}"#,
        )
        .unwrap();
        let paths = dropbox_from_info(&dropbox.join("info.json"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/Users/me/Dropbox"),
                PathBuf::from("/Users/me/Contoso")
            ]
        );
        // 没有那个文件（或写坏了）就安静地当作不知道。
        assert!(dropbox_from_info(&dropbox.join("没有.json")).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_undownloaded_icloud_file_is_cloud_only() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("iCloudDrive");
        std::fs::create_dir_all(&root).unwrap();
        let roots = vec![SyncRoot {
            provider: Provider::ICloud,
            path: root.clone(),
        }];
        let (found, state) = describe(&root.join(".notes.txt.icloud"), &roots).unwrap();
        assert_eq!(found.provider, Provider::ICloud);
        assert_eq!(state, State::CloudOnly);
        let (_, state) = describe(&root.join("notes.txt"), &roots).unwrap();
        assert_eq!(state, State::Synced);
    }
}
