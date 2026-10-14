//! 三类"算一算"的活：这个文件夹多大、哪些文件是重复的、这个文件的校验和是什么。
//!
//! 和搜索一样分成两层：这里是纯逻辑（不认识 UI、不碰 GPUI），
//! 面板那一层在 `analyze_entity.rs`，只负责把结果画出来、把中止按钮接上。
//!
//! 三条共同规矩：
//! - **全程只读**：这三类活一个字节都不写，出错最多是"没算出来"，
//!   所以它们不需要确认框，也不进撤销栈；
//! - **随时可中止**：每 [`TICK`] 个条目问一次 [`Control`]，中止后已经算出来的
//!   那部分仍然算数——半途的数字（"扫到这儿已经 8 GB 了"）本身就有用；
//! - **不跟符号链接**：目录环和 `/proc` 那一类能把扫描拖到天荒地老，
//!   而"链接指向的那个文件"本来也不该被算两次。

use crate::ops::Control;
use digest::Digest;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 多大的文件才值得找重复。默认 1 字节：空文件互相重复没有意义，
/// 而项目里动辄几百个空文件，会把结果淹掉。
pub const MIN_DUP_BYTES: u64 = 1;

/// 比对的头多少个字节。
///
/// 绝大多数不一样的文件在这 4 KB 里就分出胜负了，
/// 全量哈希只留给"头 4 KB 也一样"的少数候选——
/// 于是一个 100 GB 的目录也不用真的读满 100 GB。
const SNIFF_BYTES: u64 = 4 * 1024;

/// 大小报告里列出最大的几个文件。
///
/// 用户点"这个文件夹多大"多半是在找"到底谁占的地方最多"，
/// 给五个最大的比给一万行的清单有用。
const LARGEST: usize = 5;

/// 每扫这么多条目回一次进度。逐条回报会把消息队列淹了，
/// 而 256 条的间隔在界面上是"数字一直在动"，不会看着像卡住。
const TICK: usize = 256;

/// 读文件用的缓冲：10 GB 的文件也不该占 10 GB 内存。
const CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 文件夹大小
// ---------------------------------------------------------------------------

/// 一个文件多大。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizedFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// 一个目录算完的账。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SizeReport {
    pub root: PathBuf,
    /// 里面所有文件的字节数之和。目录自己占的那点不算：
    /// 用户问"这个文件夹多大"，想的是拷走要多久、盘还剩多少。
    pub bytes: u64,
    pub files: usize,
    /// 目录个数（含根这一个）。
    pub dirs: usize,
    /// 最大的几个文件，从大到小。
    pub largest: Vec<SizedFile>,
    /// 是不是中途被中止了。中止后的数字是"扫到那儿为止"的，仍然可用。
    pub cancelled: bool,
}

/// 扫描途中的进度：界面拿它写"已扫 3,214 项 · 1.2 GB"。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeProgress {
    pub root: PathBuf,
    pub bytes: u64,
    pub files: usize,
    pub dirs: usize,
}

/// 算一个目录（或一个文件）有多大。
///
/// 深度优先：顺序无所谓，但栈比队列省内存——十万项的目录也不会
/// 因为"待办"堆在内存里而变慢。
pub fn size_of(
    root: &Path,
    control: &Control,
    on_tick: &mut impl FnMut(SizeProgress),
) -> SizeReport {
    let mut report = SizeReport {
        root: root.to_path_buf(),
        ..Default::default()
    };
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;

    // 根是文件：算它一个，不用 read_dir（那只会拿到一个 ENOTDIR）。
    if fs::metadata(root).is_ok_and(|it| it.is_file()) {
        report.files = 1;
        report.bytes = fs::metadata(root).map_or(0, |it| it.len());
        return report;
    }

    while let Some(dir) = stack.pop() {
        if !control.should_continue() {
            report.cancelled = true;
            return report;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            // 读不了（没权限、被删了、断开的网络盘）就当这层是空的，
            // 别让一个目录把整趟扫描搞黄。
            continue;
        };
        report.dirs += 1;
        for entry in entries.flatten() {
            if !control.should_continue() {
                report.cancelled = true;
                return report;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let bytes = entry.metadata().map_or(0, |it| it.len());
            report.files += 1;
            report.bytes += bytes;
            insert_largest(
                &mut report.largest,
                SizedFile {
                    path: entry.path(),
                    bytes,
                },
            );
            seen += 1;
            if seen.is_multiple_of(TICK) {
                on_tick(SizeProgress {
                    root: report.root.clone(),
                    bytes: report.bytes,
                    files: report.files,
                    dirs: report.dirs,
                });
            }
        }
    }
    report
}

/// 把 `file` 插进"最大的几个"里，插不进去（比现有的都小）就算了。
fn insert_largest(list: &mut Vec<SizedFile>, file: SizedFile) {
    let position = list.partition_point(|it| it.bytes >= file.bytes);
    if position >= LARGEST {
        return;
    }
    list.insert(position, file);
    list.truncate(LARGEST);
}

// ---------------------------------------------------------------------------
// 重复文件
// ---------------------------------------------------------------------------

/// 找重复的口径。
#[derive(Clone, Debug)]
pub struct DupOptions {
    pub root: PathBuf,
    /// 小于这个大小的文件不看，见 [`MIN_DUP_BYTES`]。
    pub min_bytes: u64,
    /// 要不要跳过 `node_modules` 那一类噪音目录，见 [`crate::search::SKIP_DIRS`]。
    ///
    /// 默认跳：依赖目录里的重复是"装了两遍依赖"，不是用户想清理的东西，
    /// 而它们常常是几万个小文件，扫一遍能把人等老。
    pub skip_noise: bool,
}

impl DupOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            min_bytes: MIN_DUP_BYTES,
            skip_noise: true,
        }
    }
}

/// 一组内容完全一样的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupGroup {
    /// 每个文件多大（同组都一样）。
    pub bytes: u64,
    /// 内容的 SHA-256，用来给组编号，也给用户对照。
    pub digest: String,
    /// 同组的所有路径，按路径排过序：删的时候有个稳定的顺序。
    pub paths: Vec<PathBuf>,
}

impl DupGroup {
    /// 删到只剩一个能省下多少字节。
    pub fn wasted(&self) -> u64 {
        self.bytes * self.paths.len().saturating_sub(1) as u64
    }
}

/// 找完一趟的账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DupStats {
    /// 看过多少个文件（够资格参与比对的那些）。
    pub scanned: usize,
    pub groups: usize,
    /// 各组的 [`DupGroup::wasted`] 之和。
    pub wasted: u64,
    pub cancelled: bool,
}

/// 从 `root` 往下找内容完全一样的文件，一组一组交给 `on_group`。
///
/// 三道筛子依次收紧，每一步都比下一步便宜得多：
/// 1. **大小**：不一样的直接出局，一个 `stat` 就够；
/// 2. **头 4 KB**：同size 的文件先比对开头，绝大多数在这里就分开了；
/// 3. **整个文件**：只有剩下的候选才真的读完。
pub fn duplicates(
    options: &DupOptions,
    control: &Control,
    on_group: &mut impl FnMut(DupGroup),
) -> DupStats {
    let mut stats = DupStats::default();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();

    let mut stack = vec![options.root.clone()];
    while let Some(dir) = stack.pop() {
        if !control.should_continue() {
            stats.cancelled = true;
            return stats;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !control.should_continue() {
                stats.cancelled = true;
                return stats;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !options.skip_noise || !noise(&name) {
                    stack.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let bytes = entry.metadata().map_or(0, |it| it.len());
            if bytes < options.min_bytes {
                continue;
            }
            stats.scanned += 1;
            by_size.entry(bytes).or_default().push(path);
        }
    }

    for (bytes, paths) in by_size {
        if paths.len() < 2 {
            continue;
        }
        if !control.should_continue() {
            stats.cancelled = true;
            return stats;
        }
        // 第二道筛子：头 4 KB。读不出来（权限、被删了）就退出比对——
        // 宁可漏报，不能把两个读不了的文件判成"一样"。
        let mut by_sniff: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in &paths {
            if let Some(digest) = digest_prefix(path, SNIFF_BYTES) {
                by_sniff.entry(digest).or_default().push(path.clone());
            }
        }
        for paths in by_sniff.into_values() {
            if paths.len() < 2 {
                continue;
            }
            if !control.should_continue() {
                stats.cancelled = true;
                return stats;
            }
            let mut by_whole: HashMap<String, Vec<PathBuf>> = HashMap::new();
            for path in &paths {
                if let Some(digest) = digest_prefix(path, 0) {
                    by_whole.entry(digest).or_default().push(path.clone());
                }
            }
            for (digest, mut paths) in by_whole {
                if paths.len() < 2 {
                    continue;
                }
                paths.sort();
                let group = DupGroup {
                    bytes,
                    digest,
                    paths,
                };
                stats.wasted += group.wasted();
                stats.groups += 1;
                on_group(group);
            }
        }
    }
    stats
}

/// 这个目录名是不是噪音（依赖目录、构建产物那一类）。
///
/// 复用搜索那一套 [`crate::search::SKIP_DIRS`]：两处判断一致，
/// 用户不会在搜索里找不到、却在查重复时翻出一堆 `node_modules`。
fn noise(name: &str) -> bool {
    crate::search::SKIP_DIRS
        .iter()
        .any(|it| name.eq_ignore_ascii_case(it))
}

// ---------------------------------------------------------------------------
// 校验和
// ---------------------------------------------------------------------------

/// 一个文件的三个校验和。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sums {
    pub path: PathBuf,
    pub bytes: u64,
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

/// 算一个文件的三个校验和。
///
/// 三个一起算：它们都要把文件读一遍，一遍读完顺手喂给三个摘要，
/// 比各读一遍省三分之二的磁盘时间——大文件上这是分钟级的差别。
///
/// 读不出来就报原因（`Err`），中止也走同一条路：
/// 界面只需要知道"这一个没算出来"，不需要区分是权限还是用户点的中止。
pub fn checksums(path: &Path, control: &Control) -> Result<Sums, crate::i18n::Msg> {
    let mut file = fs::File::open(path)
        .map_err(|error| crate::i18n::Message::caused("读不了：{error}", error))?;
    let bytes = file.metadata().map(|it| it.len()).unwrap_or_default();
    let mut md5 = md5::Context::new();
    let mut sha1 = sha1::Sha1::new();
    let mut sha256 = sha2::Sha256::new();
    let mut buffer = vec![0u8; CHUNK];

    loop {
        if !control.should_continue() {
            return Err(crate::i18n::Message::aborted());
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| crate::i18n::Message::caused("读不了：{error}", error))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        md5.consume(chunk);
        sha1.update(chunk);
        sha256.update(chunk);
    }

    Ok(Sums {
        path: path.to_path_buf(),
        bytes,
        md5: hex(&md5.finalize().0),
        sha1: hex(sha1.finalize().as_slice()),
        sha256: hex(sha256.finalize().as_slice()),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 文件的 SHA-256（整个文件，流式读）。读不出来返回 `None`。
pub fn sha256(path: &Path) -> Option<String> {
    digest_prefix(path, 0)
}

/// 文件的 SHA-256；`limit` 大于 0 时只算开头那么多字节（`0` = 整个文件）。
///
/// 读不出来返回 `None`：查重复时"算不出来"必须和"算出来不一样"区分开，
/// 否则两个没权限读的文件会被判成一对重复。
fn digest_prefix(path: &Path, limit: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut left = if limit == 0 { u64::MAX } else { limit };

    while left > 0 {
        let want = (left.min(CHUNK as u64)) as usize;
        let read = file.read(&mut buffer[..want]).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        left -= read as u64;
    }
    Some(hex(hasher.finalize().as_slice()))
}

// ---------------------------------------------------------------------------
// 后台跑起来
// ---------------------------------------------------------------------------

/// 扫描过程中送回来的东西。
#[derive(Clone, Debug)]
pub enum SizeEvent {
    /// 进度。
    Tick(SizeProgress),
    /// 一个根目录算完了。
    Report(SizeReport),
    /// 全部算完了。
    Finished,
}

#[derive(Clone, Debug)]
pub enum DupEvent {
    /// 找到一组重复（边找边送，不用等整趟跑完）。
    Group(DupGroup),
    Finished(DupStats),
}

#[derive(Clone, Debug)]
pub enum SumEvent {
    File(Sums),
    /// 这一个没算出来：权限不够、文件没了、或者中止了。
    Failed {
        path: PathBuf,
        error: crate::i18n::Msg,
    },
    Finished,
}

/// 起一个 OS 线程干活，结果（与进度）通过 `sink` 送回来。
///
/// 返回的 [`Control`] 交给界面攥着：想中止就 `abort()`。
/// 线程起不来（进程里线程太多）就当场跑完——慢一点，但界面不会一直转圈，
/// 所以 `work` 得能跑两回的可能：用 `Arc` 包一层，谁先拿到谁跑。
pub fn spawn<T: Send + 'static>(
    sink: Arc<dyn Fn(T) + Send + Sync>,
    work: impl Fn(&Control, &dyn Fn(T)) + Send + Sync + 'static,
) -> Control {
    let control = Control::new();
    let work = Arc::new(work);
    let for_thread = work.clone();
    let flag = control.clone();
    let sink2 = sink.clone();
    let started = std::thread::Builder::new()
        .name("wfiles-analyze".to_string())
        .spawn(move || for_thread.as_ref()(&flag, sink2.as_ref()));
    if started.is_err() {
        work.as_ref()(&control, sink.as_ref());
    }
    control
}

/// 依次算几个目录的大小。中止后剩下的就不算了。
pub fn spawn_sizes(roots: Vec<PathBuf>, sink: Arc<dyn Fn(SizeEvent) + Send + Sync>) -> Control {
    spawn(sink, move |control, send| {
        for root in roots.iter() {
            let report = size_of(root, control, &mut |progress| {
                send(SizeEvent::Tick(progress))
            });
            let cancelled = report.cancelled;
            send(SizeEvent::Report(report));
            if cancelled {
                break;
            }
        }
        send(SizeEvent::Finished);
    })
}

pub fn spawn_duplicates(options: DupOptions, sink: Arc<dyn Fn(DupEvent) + Send + Sync>) -> Control {
    spawn(sink, move |control, send| {
        let stats = duplicates(&options, control, &mut |group| send(DupEvent::Group(group)));
        send(DupEvent::Finished(stats));
    })
}

pub fn spawn_checksums(paths: Vec<PathBuf>, sink: Arc<dyn Fn(SumEvent) + Send + Sync>) -> Control {
    spawn(sink, move |control, send| {
        for path in paths.iter() {
            if !control.should_continue() {
                break;
            }
            match checksums(path, control) {
                Ok(sums) => send(SumEvent::File(sums)),
                Err(error) => send(SumEvent::Failed {
                    path: path.clone(),
                    error,
                }),
            }
        }
        send(SumEvent::Finished);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("建目录");
        }
        let mut file = fs::File::create(path).expect("建文件");
        file.write_all(contents.as_bytes()).expect("写文件");
    }

    /// 一个中止不掉也暂停不了的把手：单测里不用管它。
    fn go() -> Control {
        Control::new()
    }

    fn size(root: &Path) -> SizeReport {
        size_of(root, &go(), &mut |_| {})
    }

    // ---- 文件夹大小 ----

    #[test]
    fn a_folder_size_is_the_sum_of_its_files() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("a.txt"), "12345");
        write(root.path().join("deep/b.txt"), "123");
        let report = size(root.path());
        assert_eq!(report.bytes, 8);
        assert_eq!(report.files, 2);
        // 根目录 + deep
        assert_eq!(report.dirs, 2);
        assert!(!report.cancelled);
    }

    #[test]
    fn a_single_file_can_be_measured_too() {
        let root = TempDir::new().expect("临时目录");
        let file = root.path().join("a.txt");
        write(file.clone(), "12345");
        let report = size(&file);
        assert_eq!(report.bytes, 5);
        assert_eq!(report.files, 1);
        assert_eq!(report.dirs, 0, "它是个文件，不是一层目录");
    }

    #[test]
    fn the_largest_files_come_out_biggest_first_and_capped() {
        let root = TempDir::new().expect("临时目录");
        for index in 0..12 {
            write(
                root.path().join(format!("file{index}.bin")),
                &"x".repeat(index),
            );
        }
        let report = size(root.path());
        let sizes: Vec<u64> = report.largest.iter().map(|it| it.bytes).collect();
        assert_eq!(sizes, vec![11, 10, 9, 8, 7], "最大的五个，从大到小");
    }

    #[test]
    fn an_unreadable_directory_does_not_ruin_the_walk() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("a.txt"), "12345");
        // 一个已经不在了的目录：read_dir 会失败，扫描该继续而不是崩。
        let report = size(&root.path().join("并不存在"));
        assert_eq!(report.bytes, 0);
        assert!(!report.cancelled);
    }

    #[test]
    fn aborting_stops_the_walk_and_says_so() {
        // 得扫到 256 个以上才会回一次进度：进度回调里中止，
        // 正好模拟"用户扫到一半点了停止"。
        let root = TempDir::new().expect("临时目录");
        for index in 0..400 {
            write(root.path().join(format!("file{index}.txt")), "1234567890");
        }
        let control = Control::new();
        let mut ticks = 0;
        let report = size_of(root.path(), &control, &mut |_| {
            ticks += 1;
            control.abort();
        });
        assert!(report.cancelled, "中止过的要写明，界面才知道这是半截的数");
        assert!(report.files < 400, "不该把 400 个都扫完：{report:?}");
        assert!(ticks >= 1, "至少回过一次进度");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed() {
        let root = TempDir::new().expect("临时目录");
        fs::create_dir_all(root.path().join("a")).expect("建目录");
        write(root.path().join("a/inside.txt"), "123");
        std::os::unix::fs::symlink(root.path().join("a"), root.path().join("a/loop"))
            .expect("建链接");
        let report = size(root.path());
        assert_eq!(report.files, 1, "链接不该被当成一个真的文件：{report:?}");
        assert_eq!(report.dirs, 2);
    }

    // ---- 重复文件 ----

    fn dups(root: &Path) -> (Vec<DupGroup>, DupStats) {
        let mut groups = Vec::new();
        let stats = duplicates(&DupOptions::new(root.to_path_buf()), &go(), &mut |group| {
            groups.push(group)
        });
        (groups, stats)
    }

    #[test]
    fn identical_files_in_different_folders_are_reported() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("a/one.txt"), "同样的内容");
        write(root.path().join("b/two.txt"), "同样的内容");
        let (groups, stats) = dups(root.path());
        assert_eq!(groups.len(), 1, "该找到一组：{groups:?}");
        assert_eq!(groups[0].paths.len(), 2);
        assert_eq!(groups[0].bytes, 15);
        assert_eq!(stats.wasted, 15, "删掉一个省 15 字节");
    }

    #[test]
    fn files_of_the_same_size_but_different_content_are_not_duplicates() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("a.txt"), "aaaa");
        write(root.path().join("b.txt"), "bbbb");
        let (groups, _) = dups(root.path());
        assert!(groups.is_empty(), "同size不同内容不该被判成重复");
    }

    #[test]
    fn files_that_only_share_their_first_block_are_not_duplicates() {
        let root = TempDir::new().expect("临时目录");
        // 头 4 KB 完全一样，后面不同：这正是"只比开头"会误判的那种。
        let head = "x".repeat(4096);
        let mut a = head.clone();
        a.push_str("AAA");
        let mut b = head.clone();
        b.push_str("BBB");
        write(root.path().join("a.bin"), &a);
        write(root.path().join("b.bin"), &b);
        let (groups, _) = dups(root.path());
        assert!(groups.is_empty(), "头 4 KB 一样不够，得整个文件一样");
    }

    #[test]
    fn empty_files_are_not_duplicates_of_each_other() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("a.txt"), "");
        write(root.path().join("b.txt"), "");
        let (groups, stats) = dups(root.path());
        assert!(groups.is_empty(), "空文件互相重复没有意义");
        assert_eq!(stats.scanned, 0, "压根没进比对");
    }

    #[test]
    fn noisy_directories_are_skipped_by_default() {
        let root = TempDir::new().expect("临时目录");
        write(root.path().join("node_modules/pkg/a.js"), "同样的内容");
        write(root.path().join("src/b.js"), "同样的内容");
        let (groups, _) = dups(root.path());
        assert!(groups.is_empty(), "噪音目录里的重复不该报：{groups:?}");

        // 但用户要看的时候能看：关掉这一刀就找得出来。
        let mut options = DupOptions::new(root.path().to_path_buf());
        options.skip_noise = false;
        let mut groups = Vec::new();
        duplicates(&options, &go(), &mut |group| groups.push(group));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn three_copies_count_as_one_group_and_two_wasted() {
        let root = TempDir::new().expect("临时目录");
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(root.path().join(name), "12345");
        }
        let (groups, stats) = dups(root.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 3);
        assert_eq!(stats.wasted, 10, "三个副本多占两份");
    }

    #[test]
    fn a_group_lists_its_paths_in_a_stable_order() {
        let root = TempDir::new().expect("临时目录");
        for name in ["c.txt", "a.txt", "b.txt"] {
            write(root.path().join(name), "12345");
        }
        let (groups, _) = dups(root.path());
        let names: Vec<String> = groups[0]
            .paths
            .iter()
            .map(|path| {
                path.file_name()
                    .map(|it| it.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(names, ["a.txt", "b.txt", "c.txt"]);
    }

    // ---- 校验和 ----

    #[test]
    fn the_three_sums_of_a_known_string() {
        let root = TempDir::new().expect("临时目录");
        let path = root.path().join("abc.txt");
        write(path.clone(), "abc");
        let sums = checksums(&path, &go()).expect("算得出");
        assert_eq!(sums.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sums.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sums.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sums.bytes, 3);
    }

    #[test]
    fn a_file_bigger_than_the_buffer_is_hashed_whole() {
        let root = TempDir::new().expect("临时目录");
        let path = root.path().join("big.bin");
        let contents = vec![b'z'; CHUNK * 3 + 17];
        fs::write(&path, &contents).expect("写文件");
        let sums = checksums(&path, &go()).expect("算得出");
        let expected = hex(sha2::Sha256::digest(&contents).as_slice());
        assert_eq!(sums.sha256, expected, "分块读出来的要和一次性算的一样");
        assert_eq!(sums.bytes, contents.len() as u64);
    }

    #[test]
    fn a_file_that_cannot_be_read_says_why() {
        let sums = checksums(Path::new("/并不存在的文件"), &go());
        assert!(sums.is_err(), "读不了就该报错，而不是给个空字符串");
        // 判中文原文：`Display` 恒给中文，跟语言无关。
        // **别给 `Message` 加 `contains`**——那正是 `is()` 要顶掉的坑：
        // 拿显示文案当判据，切到英文就永远不成立。
        assert!(sums.unwrap_err().to_string().contains("读不了"));
    }

    #[test]
    fn aborting_a_checksum_reports_it() {
        let root = TempDir::new().expect("临时目录");
        let path = root.path().join("big.bin");
        fs::write(&path, vec![b'z'; CHUNK * 4]).expect("写文件");
        let control = Control::new();
        control.abort();
        let error = checksums(&path, &control).expect_err("该中止");
        // 判 key 不判文案：这条判据在英文下也必须成立。
        assert!(error.is_aborted(), "实际错误：{error}");
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
    }
}
