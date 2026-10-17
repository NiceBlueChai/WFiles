//! 侧栏的「磁盘」区：列出本机挂着的卷（外接硬盘、U 盘、光盘、网络共享）。
//!
//! 为什么只做这一层：**可移动设备不做专用传输 UI**。U 盘插上就是一个普通
//! 目录，拷贝、拖放、重命名、回收站全都跟本地盘一模一样——那才是用户在
//! 系统自带的文件管理器里已经会用的那一套，为它另做一套传输界面只会
//! 多一处行为不一致的地方。
//!
//! 三个平台各自的落点不一样，都列在 [`roots`] 里：macOS 是 `/Volumes`
//! 底下每个已挂载卷一个目录；Windows 是盘符；Linux 散在 `/media`、
//! `/run/media/<用户>` 和 `/mnt` 底下。
//!
//! 为什么**不用**文件监控：插拔设备在三个平台上给出的事件各不相同
//! （macOS 走 diskarbitration、Linux 走 udev、什么通知都没有的场合更多），
//! 为一个侧栏列表接三套平台 API 不划算；几秒钟的延迟用户根本察觉不到。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 重新扫一遍的间隔。
///
/// 卷会热插拔，所以不能像云盘那样永久缓存；但也不能每一帧都扫——
/// Windows 上那是一次 26 个盘符的探测，而侧栏是跟着光标重绘的。
/// 三秒钟：插上 U 盘，抬眼就能看见；人不动的时候几乎不干活。
const RESCAN_AFTER: Duration = Duration::from_secs(3);

/// 一个挂着的卷。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// 给用户看的名字。macOS/Linux 上就是卷标（挂载点目录名）。
    pub name: String,
    /// 挂载点，也就是导航时用的路径。
    pub path: PathBuf,
}

impl Volume {
    /// 读屏念的整句：光念「WD」不知道是什么，得说清是块盘。
    pub fn spoken(&self) -> String {
        format!("磁盘：{}", self.name)
    }
}

/// 缓存里的一份快照。`Arc` 是为了让 [`detected`] 每次只多一次原子自增，
/// 而不是每帧把那几个卷深拷一遍。
type Snapshot = Arc<Vec<Volume>>;

fn cache() -> &'static Mutex<(Instant, Snapshot)> {
    static CACHE: OnceLock<Mutex<(Instant, Snapshot)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        // 起手就标成"过期"，第一次调用老老实实扫一遍。
        Mutex::new((Instant::now() - RESCAN_AFTER, Arc::new(Vec::new())))
    })
}

/// 测试里指定"本机挂着哪些卷"。
#[cfg(test)]
static OVERRIDE: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();

#[cfg(test)]
fn override_slot() -> &'static Mutex<Option<Snapshot>> {
    OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// 本机现在挂着哪些卷，带三秒缓存。
pub fn detected() -> Snapshot {
    #[cfg(test)]
    if let Ok(guard) = override_slot().lock()
        && let Some(fixed) = guard.as_ref()
    {
        return Arc::clone(fixed);
    }
    let Ok(mut guard) = cache().lock() else {
        // 锁被 panic 弄脏了：这一帧给个空的，别把界面卡住。
        return Arc::new(Vec::new());
    };
    if guard.0.elapsed() >= RESCAN_AFTER {
        guard.1 = Arc::new(scan());
        guard.0 = Instant::now();
    }
    Arc::clone(&guard.1)
}

/// 测试专用：钉死"本机挂着哪些卷"。
///
/// 没有它就没法在 CI 上验侧栏那几行——跑测试的机器上一个 U 盘都没有，
/// 真扫永远是空的。传空列表就是"一个卷都没挂"。
#[cfg(test)]
pub fn set_override(volumes: Vec<Volume>) {
    if let Ok(mut guard) = override_slot().lock() {
        *guard = Some(Arc::new(volumes));
    }
}

/// 测试专用：交还给真扫。
#[cfg(test)]
pub fn clear_override() {
    if let Ok(mut guard) = override_slot().lock() {
        *guard = None;
    }
}

/// 丢掉缓存，下一次 [`detected`] 立刻重扫。
///
/// 插拔设备本身没有跨平台的可靠通知（见模块注释），所以给界面留一个
/// 主动的入口：用户点了刷新就调它。
pub fn rescan_now() {
    if let Ok(mut guard) = cache().lock() {
        guard.0 = Instant::now() - RESCAN_AFTER;
    }
}

/// 真去扫一遍。测试直接调它（绕过缓存）。
pub fn scan() -> Vec<Volume> {
    let mut found = Vec::new();
    #[cfg(target_os = "windows")]
    found.extend(drive_letters());
    for root in roots() {
        found.extend(mounted_under(&root));
    }
    finish(found)
}

/// 排序 + 按挂载点去重。列表给谁看都走这里，顺序在这里定死。
fn finish(mut volumes: Vec<Volume>) -> Vec<Volume> {
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    // 同一个挂载点可能从两条路被找到（管理员把 `/media/USB` 软链接到
    // `/mnt/USB`），列两行会让用户以为有两块一样的盘。
    // 名字就是从路径末段取的，所以同名必定相邻，`dedup_by` 够用。
    volumes.dedup_by(|a, b| a.path == b.path);
    volumes
}

/// 本机上，卷会出现在哪儿。
/// 这一台机器的文件系统根——侧栏左下那颗「本地文件系统」点了去哪儿。
///
/// 这里给的是"整台机器"那一层，不是某一个卷：
///
/// - macOS / Linux：`/`，一切挂载点的父目录。
/// - Windows：没有统一的根，按惯例取系统盘（`%SystemDrive%`）；环境变量
///   拿不到就退回 `C:\`——绝大多数机器上它就是系统盘，而真改过盘符的
///   机器在侧栏的磁盘区里另有入口。
///
/// 为什么不干脆列"卷"：那是 [`roots`] + [`mounted_under`]（侧栏的磁盘区）
/// 已经在做的事。这一颗的用途是"无论在哪儿，一步回到本机"，也就是从远程
/// 连接里退回来的那条路——所以它得是一个真实、唯一、跨平台都有的落点。
pub fn machine_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let drive = std::env::var("SystemDrive")
            .ok()
            .filter(|it| !it.is_empty())
            .unwrap_or_else(|| "C:".to_string());
        PathBuf::from(format!("{drive}\\"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/")
    }
}

fn roots() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // macOS 把每一个挂载的卷（外接盘、U 盘、光盘、网络共享）都放进
        // `/Volumes`，启动盘不在这里——所以这里列出来的天然都是"额外的盘"。
        vec![PathBuf::from("/Volumes")]
    }
    #[cfg(target_os = "linux")]
    {
        // 只列 `/media/<用户>` 而不是 `/media` 本身：`/media` 底下那一层
        // 是用户名（发行版的 fstab 自动挂载惯例），拿它当卷会把"alice"
        // 列为一块盘。`/run/media` 同理。
        let mut roots = vec![PathBuf::from("/mnt")];
        if let Some(user) = whoami() {
            roots.push(PathBuf::from("/media").join(&user));
            roots.push(PathBuf::from("/run/media").join(&user));
        } else {
            // 认不出用户名就退回裸目录：多数场合 `/media` 下直接就是盘。
            roots.push(PathBuf::from("/media"));
        }
        roots
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Windows 不用目录树，走盘符。
        Vec::new()
    }
}

/// `root` 底下每一个挂载点。
///
/// **顺序不保证**（`read_dir` 本来就不保证），要有序请过 [`finish`]。
///
/// 只看一层：挂载点不会再互相嵌套成“卷里套卷”，而递归下去会顺着用户
/// 自己建的目录一路走到文件系统根。
fn mounted_under(root: &Path) -> Vec<Volume> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` 不跟随软链接：跟着 iDisk 之类的软链接走进去，
        // 会把网络目录当本地盘列出来。只认真目录。
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let Some(name) = mount_label(&path) else {
            continue;
        };
        out.push(Volume { name, path });
    }
    out
}

/// 挂载点的名字；`None` 表示这个目录不算卷。
fn mount_label(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.is_empty() {
        return None;
    }
    // macOS 往 `/Volumes` 里放过隐藏的 `.vol` 辅助卷（Time Machine 的
    // 快照挂点就是 `.MobileBackups`）。那不是一块盘，别列给用户看。
    if name.starts_with('.') {
        return None;
    }
    Some(name)
}

/// Windows 上按盘符找卷。其余平台是空实现（走 [`roots`] + [`mounted_under`]）。
#[cfg(target_os = "windows")]
fn drive_letters() -> Vec<Volume> {
    // A 到 Z 扫一圈看着浪费，其实软驱、光驱、空读卡器槽位
    // 都在 `is_dir` 那一步被滤掉了。
    ('A'..='Z')
        .filter_map(|letter| {
            let path = PathBuf::from(format!("{}:\\", letter));
            path.is_dir().then(|| Volume {
                // 卷标得调 Win32 的 `GetVolumeInformation`，得为此引一个
                // Windows API 依赖。盘符本身已经够用户认得了，
                // 真要看卷标路径栏里也有。
                name: format!("{}:", letter),
                path,
            })
        })
        .collect()
}

/// 当前用户名。Linux 的自动挂载目录多一层用户名，得认出来才能往下找。
#[cfg(target_os = "linux")]
fn whoami() -> Option<String> {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|it| !it.is_empty())
}

#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 碰全局 override 的测试要串着来——那是一份进程级状态。
///
/// UI 集成测试也用同一个锁：它们和这里的单测是两个线程池，
/// 但改的是同一份 override，不共用一把锁就会互相盖。
#[cfg(test)]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 本机根是这个入口对外唯一的承诺：**绝对路径**，而且真进得去。
    #[test]
    fn the_machine_root_is_a_real_absolute_directory() {
        let root = machine_root();
        assert!(root.is_absolute(), "{} 不是绝对路径", root.display());
        assert!(root.is_dir(), "{} 进不去", root.display());
    }

    /// unix 上就是 `/`——不是 `/Volumes`，也不是家目录。
    #[cfg(unix)]
    #[test]
    fn on_unix_the_machine_root_is_the_slash() {
        assert_eq!(machine_root(), Path::new("/"));
    }

    /// Windows 扫盘符没法在 macOS 上验，跨平台的那部分逻辑放在
    /// [`mounted_under`] 里测——那才是真正在干活的地方。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_mounted_volume_is_named_after_its_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("我的盘")).unwrap();
        fs::create_dir(root.path().join("Backup")).unwrap();
        fs::write(root.path().join("不是卷.txt"), b"x").unwrap();

        let found = mounted_under(root.path());
        let mut names: Vec<&str> = found.iter().map(|it| it.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Backup", "我的盘"], "只收目录，文件不算卷");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_mount_point_is_reached_just_like_any_folder() {
        let root = tempfile::tempdir().unwrap();
        let disk = root.path().join("USB");
        fs::create_dir(&disk).unwrap();
        fs::write(disk.join("a.txt"), b"hello").unwrap();

        let found = mounted_under(root.path());
        let volume = found.iter().find(|it| it.name == "USB").unwrap();
        // 这就是"不做专用传输 UI"的落点：挂载点是普通路径，
        // 能列、能读、能拷，跟系统自带的那个一个样。
        assert!(volume.path.is_dir());
        assert_eq!(fs::read(volume.path.join("a.txt")).unwrap(), b"hello");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_missing_root_is_not_an_error() {
        // 没插过盘的机器上 `/media`、`/run/media` 可能压根不存在。
        assert!(mounted_under(Path::new("/压根不存在的目录")).is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn hidden_helper_volumes_are_not_shown_to_the_user() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".MobileBackups")).unwrap();
        fs::create_dir(root.path().join("真盘")).unwrap();

        let found = mounted_under(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "真盘");
    }

    #[test]
    fn the_same_mount_is_not_listed_twice() {
        // 管理员把一处软链接成另一处时，两条路径都能扫到同一个盘。
        let twice = vec![
            Volume {
                name: "USB".into(),
                path: PathBuf::from("/mnt/USB"),
            },
            Volume {
                name: "USB".into(),
                path: PathBuf::from("/media/USB"),
            },
        ];
        assert_eq!(finish(twice).len(), 2, "挂载点不同就是两块盘");

        let same = vec![
            Volume {
                name: "USB".into(),
                path: PathBuf::from("/mnt/USB"),
            },
            Volume {
                name: "USB".into(),
                path: PathBuf::from("/mnt/USB"),
            },
        ];
        assert_eq!(finish(same).len(), 1, "同一个挂载点只列一行");
    }

    #[test]
    fn the_cache_eventually_refreshes() {
        let _serial = test_guard();
        // 插拔没有跨平台通知，这一小段缓存是唯一的折中：
        // 至少得保证它真的会过期，而不是永远拿着第一次的结果。
        // 先交还给真扫——这个测试验的正是真扫描那条路。
        clear_override();
        assert!(RESCAN_AFTER <= Duration::from_secs(5));
        rescan_now();
        let first = detected();
        let second = detected();
        assert!(
            Arc::ptr_eq(&first, &second),
            "没到重扫时限就该直接复用同一份，不重新扫盘"
        );
    }

    #[test]
    fn an_empty_machine_shows_no_volumes() {
        let _serial = test_guard();
        set_override(Vec::new());
        assert!(detected().is_empty());
        clear_override();
    }

    #[test]
    fn the_override_wins_over_whatever_this_machine_really_has() {
        let _serial = test_guard();
        set_override(vec![Volume {
            name: "USB".into(),
            path: PathBuf::from("/Volumes/USB"),
        }]);
        let found = detected();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "USB");
        assert_eq!(found[0].spoken(), "磁盘：USB");
        clear_override();
    }

    #[test]
    fn scanning_this_machine_survives_whatever_is_mounted() {
        // 真的扫一遍本机：不该 panic，也不该把用户 home 之类的东西
        // 误当成卷（/mnt 或 /media 底下什么都有可能）。
        let _ = scan();
    }
}
