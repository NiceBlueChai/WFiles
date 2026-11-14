//! 目录比较与同步计划：两边各有什么、哪边新、同步要动哪些文件。
//!
//! 这一层不认识 UI，也不真的动文件：它只回答两个问题——
//! "两边差在哪"，以及"要同步的话得拷哪些、删哪些、哪些不敢动"。
//! 真正的搬运交给界面层的 `Ops` 实体：那边有进度、有冲突处理、
//! 有撤销，没必要再写一套。
//!
//! 三条规矩：
//! - **只报差异**：一样的文件几万个也要算，但不进列表——
//!   用户打开比较是为了看不一样的地方，个数记在 [`Stats::same`] 里；
//! - **目录不单独成行**：一个目录只在一边有，它里面的文件会各自成行，
//!   把目录再列一遍只是重复；
//! - **不敢动的一定报出来**：两边都改过（时间一样或读不到时间）时不猜谁赢，
//!   列进 [`Plan::skipped`] 让人自己看。

use crate::analyze;
use crate::ops::Control;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// 一次比较最多列多少条差异。几万条的清单没人看得完，
/// 而它们得先全进内存才能显示——不如早停，并如实报"到上限了"。
pub const MAX_ENTRIES: usize = 5_000;

/// 攒够这么多条才往回发一批。
const BATCH: usize = 64;

/// 修改时间差在这么多以内算"一样"。
///
/// 拷一份文件出来，两边的 mtime 天然差几毫秒；FAT/exFAT 与不少网络盘
/// 的时间戳精度本来就是 2 秒。逐纳秒比等于把"复制出来的同一份文件"
/// 全部报成不一样，那这份清单就没法看了。真要分毫不差，用内容比对。
const TIME_TOLERANCE: Duration = Duration::from_secs(2);

/// 两边哪一边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// 比较的口径。
#[derive(Clone, Debug)]
pub struct Options {
    pub left: PathBuf,
    pub right: PathBuf,
    /// 用内容（SHA-256）判"一样不一样"，而不是只看大小和时间。
    ///
    /// 慢（要把两边都读一遍），但只有它说得准：
    /// 大小一样、时间被改成一样的文件，只看元数据会被判成"一样"。
    pub by_content: bool,
    /// 跳不跳 `node_modules` 那一类噪音目录，与搜索、查重复同一张表。
    pub skip_noise: bool,
    /// 列表最多多少条，见 [`MAX_ENTRIES`]。
    pub limit: usize,
}

impl Options {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self {
            left,
            right,
            by_content: false,
            skip_noise: true,
            limit: MAX_ENTRIES,
        }
    }
}

/// 一边上一个文件的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub bytes: u64,
    pub modified: Option<SystemTime>,
    /// 内容的 SHA-256；只在 [`Options::by_content`] 时算。
    pub digest: Option<String>,
}

/// 一条差异。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 相对两边的共同路径，列表里靠它对齐两边。
    pub path: PathBuf,
    pub status: Status,
    /// 两边都有但不一样时，哪边更新。时间一样或读不到就是 `None`——
    /// 那时候不敢自动盖，交给用户看。
    pub newer: Option<Side>,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
}

impl Entry {
    /// 两边都有的那一条在 `side` 上的绝对路径。
    pub fn on(&self, side: Side, left: &Path, right: &Path) -> Option<PathBuf> {
        let exists = match side {
            Side::Left => self.left.is_some(),
            Side::Right => self.right.is_some(),
        };
        if !exists {
            return None;
        }
        let root = match side {
            Side::Left => left,
            Side::Right => right,
        };
        Some(root.join(&self.path))
    }
}

/// 一条差异的性质。
///
/// "一样"不在这里：两个目录里一样的那部分可能几万项，
/// 列出来只会把真正要看的东西淹掉，所以只记个数（[`Stats::same`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// 两边都有，但不一样。
    Different,
    /// 只有左边有。
    LeftOnly,
    /// 只有右边有。
    RightOnly,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Different => "不同",
            Self::LeftOnly => "只有左",
            Self::RightOnly => "只有右",
        }
    }
}

/// 比完一趟的账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 两边一样的条目数（不进列表，只计数）。
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
    /// 撞到 [`MAX_ENTRIES`] 提前停了。
    pub truncated: bool,
    pub cancelled: bool,
}

impl Stats {
    /// 有差异的条目总数。
    pub fn diffs(&self) -> usize {
        self.different + self.left_only + self.right_only
    }

    /// 一句话结论。**文案归 core**：GUI 与 NSH 各自画，但说的是同一句话。
    ///
    /// 一样的那些不进列表（几万项会把要看的东西淹掉），所以它们只在
    /// 括号里报个数——用户需要知道"比过了"，但不需要看见它们。
    pub fn summary(&self) -> String {
        if self.diffs() == 0 {
            return format!("两边一样，共 {} 项", self.same);
        }
        let mut parts: Vec<String> = Vec::new();
        if self.different > 0 {
            parts.push(format!("不同 {} 项", self.different));
        }
        if self.left_only > 0 {
            parts.push(format!("只有左 {} 项", self.left_only));
        }
        if self.right_only > 0 {
            parts.push(format!("只有右 {} 项", self.right_only));
        }
        let mut text = parts.join(" · ");
        if self.same > 0 {
            text.push_str(&format!("（另有 {} 项一样）", self.same));
        }
        if self.truncated {
            text.push_str("，已到上限");
        }
        text
    }
}

/// 走一遍两边，把差异一批一批交给 `on_batch`。
///
/// 先把两边各自的"相对路径 → 样子"收齐再比对：
/// 于是左右谁先扫完都无所谓，比对只看键，顺序由 `BTreeMap` 排定，
/// 每次比出来的顺序都一样（用户来回切筛选时列表不会跳）。
pub fn compare(
    options: &Options,
    control: &Control,
    on_batch: &mut impl FnMut(Vec<Entry>),
) -> Stats {
    let mut stats = Stats::default();
    let mut left: BTreeMap<PathBuf, Meta> = BTreeMap::new();
    let mut right: BTreeMap<PathBuf, Meta> = BTreeMap::new();

    collect(&options.left, options, control, &mut left, &mut stats);
    if stats.cancelled {
        return stats;
    }
    collect(&options.right, options, control, &mut right, &mut stats);
    if stats.cancelled {
        return stats;
    }

    let mut batch: Vec<Entry> = Vec::with_capacity(BATCH);
    let mut listed = 0usize;
    // 两边键的并集：`BTreeSet` 顺手把顺序排定、把重复去掉，
    // 于是每次比出来的顺序都一样（用户来回切筛选时列表不会跳）。
    let keys: BTreeSet<&PathBuf> = left.keys().chain(right.keys()).collect();

    for path in keys {
        if listed >= options.limit {
            stats.truncated = true;
            break;
        }
        let left_meta = left.get(path.as_path()).cloned();
        let right_meta = right.get(path.as_path()).cloned();
        let entry = match (&left_meta, &right_meta) {
            (Some(left_meta), Some(right_meta)) => {
                if alike(left_meta, right_meta, options.by_content) {
                    stats.same += 1;
                    continue;
                }
                stats.different += 1;
                Entry {
                    path: path.clone(),
                    status: Status::Different,
                    newer: newer(left_meta, right_meta),
                    left: Some(left_meta.clone()),
                    right: Some(right_meta.clone()),
                }
            }
            (Some(_), None) => {
                stats.left_only += 1;
                Entry {
                    path: path.clone(),
                    status: Status::LeftOnly,
                    newer: Some(Side::Left),
                    left: left_meta.clone(),
                    right: None,
                }
            }
            (None, Some(_)) => {
                stats.right_only += 1;
                Entry {
                    path: path.clone(),
                    status: Status::RightOnly,
                    newer: Some(Side::Right),
                    left: None,
                    right: right_meta.clone(),
                }
            }
            (None, None) => continue,
        };
        listed += 1;
        batch.push(entry);
        if batch.len() >= BATCH {
            on_batch(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    stats
}

/// 走一遍 `root`，把文件按相对路径记进 `out`。
fn collect(
    root: &Path,
    options: &Options,
    control: &Control,
    out: &mut BTreeMap<PathBuf, Meta>,
    stats: &mut Stats,
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if !control.should_continue() {
            stats.cancelled = true;
            return;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !control.should_continue() {
                stats.cancelled = true;
                return;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if crate::entry_kind::is_link_or_junction(&entry) {
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
            let metadata = entry.metadata().ok();
            let relative = path
                .strip_prefix(root)
                .map(|it| it.to_path_buf())
                .unwrap_or_else(|_| path.clone());
            out.insert(
                relative,
                Meta {
                    bytes: metadata.as_ref().map_or(0, |it| it.len()),
                    modified: metadata.as_ref().and_then(|it| it.modified().ok()),
                    digest: options.by_content.then(|| analyze::sha256(&path)).flatten(),
                },
            );
        }
    }
}

/// 两边这一条算不算一样。
///
/// 按内容比时有哈希就比哈希（最准）；按元数据比时先看时间、
/// 时间读不到才退回比大小——大小一样不等于内容一样，但那时候没有更好的依据。
fn alike(left: &Meta, right: &Meta, by_content: bool) -> bool {
    if by_content {
        return match (&left.digest, &right.digest) {
            (Some(left), Some(right)) => left == right,
            // 算不出来就没法说"一样"：这时候当不同，用户至少看得到它。
            _ => false,
        };
    }
    match time_gap(left.modified, right.modified) {
        // 时间差在容差内，且大小也一样：当成同一份。
        // 大小不一样那是真的不一样（改字节不改 mtime 的工具确实存在）。
        Some(gap) => gap <= TIME_TOLERANCE && left.bytes == right.bytes,
        // 读不到时间（某些网络盘）就只比大小。
        None => left.bytes == right.bytes,
    }
}

/// 哪边更新。时间差在容差内、或者读不到时间，就回答不了（`None`）。
fn newer(left: &Meta, right: &Meta) -> Option<Side> {
    let gap = time_gap(left.modified, right.modified)?;
    if gap <= TIME_TOLERANCE {
        return None;
    }
    match (left.modified, right.modified) {
        (Some(left), Some(right)) if left >= right => Some(Side::Left),
        (Some(_), Some(_)) => Some(Side::Right),
        _ => None,
    }
}

/// 两个时间差多少。有一边读不到时间就是 `None`。
fn time_gap(left: Option<SystemTime>, right: Option<SystemTime>) -> Option<Duration> {
    let (left, right) = (left?, right?);
    if left >= right {
        left.duration_since(right).ok()
    } else {
        right.duration_since(left).ok()
    }
}

fn noise(name: &str) -> bool {
    crate::search::SKIP_DIRS
        .iter()
        .any(|it| name.eq_ignore_ascii_case(it))
}

// ---------------------------------------------------------------------------
// 同步计划
// ---------------------------------------------------------------------------

/// 同步的四种口径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// 左边独有的、左边更新的，都拷到右边。
    ToRight,
    /// 右边独有的、右边更新的，都拷到左边。
    ToLeft,
    /// 两边各把新的盖到对面。
    BothWays,
    /// 拷到右边之外，右边多出来的移到回收站。
    MirrorToRight,
    /// 拷到左边之外，左边多出来的移到回收站。
    MirrorToLeft,
}

impl SyncMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::ToRight => "→ 拷到右",
            Self::ToLeft => "← 拷到左",
            Self::BothWays => "双向：新的盖旧的",
            Self::MirrorToRight => "镜像到右（含删除）",
            Self::MirrorToLeft => "镜像到左（含删除）",
        }
    }

    /// 这一边是"源"还是"目标"。
    fn pushes(self, side: Side) -> bool {
        match self {
            Self::ToRight => side == Side::Left,
            Self::ToLeft => side == Side::Right,
            Self::BothWays => true,
            Self::MirrorToRight => side == Side::Left,
            Self::MirrorToLeft => side == Side::Right,
        }
    }

    /// 对面多出来的要不要删掉（走回收站）。
    fn removes(self, side: Side) -> bool {
        matches!(
            (self, side),
            (Self::MirrorToRight, Side::Right) | (Self::MirrorToLeft, Side::Left)
        )
    }
}

/// 要拷的一个文件：从 `from` 拷到 `to_dir` 下（目录得先建好）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    pub from: PathBuf,
    pub to_dir: PathBuf,
}

/// 不敢自动处理的那一条，连同原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip {
    pub path: PathBuf,
    pub why: String,
}

/// 一份同步计划：拷哪些、删哪些、哪些不敢动。
///
/// 计划是纯数据，界面可以先把它显示出来再动手——
/// "点了同步之后到底会动哪些文件"这件事，必须能事先看见。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub copies: Vec<Copy>,
    /// 镜像模式下对面多出来的那些；执行时走回收站，不是直接删。
    pub removals: Vec<PathBuf>,
    pub skipped: Vec<Skip>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.copies.is_empty() && self.removals.is_empty() && self.skipped.is_empty()
    }
}

/// 按 `mode` 把差异变成一份计划。
///
/// 只信两件事：一边独有的（只能从这边拷过去），以及两边都有、
/// 但明确有一边更新的。两边都改过（时间分不出来）的一律跳过并写明原因——
/// 猜错方向就是覆盖掉用户刚改的东西，那是最难解释的一类数据丢失。
pub fn plan(entries: &[Entry], left: &Path, right: &Path, mode: SyncMode) -> Plan {
    let mut out = Plan::default();
    for entry in entries {
        match entry.status {
            Status::LeftOnly => {
                let Some(path) = entry.on(Side::Left, left, right) else {
                    continue;
                };
                if mode.pushes(Side::Left) {
                    out.copies.push(Copy {
                        from: path.clone(),
                        to_dir: destination(&entry.path, right),
                    });
                } else if mode.removes(Side::Left) {
                    out.removals.push(path);
                }
            }
            Status::RightOnly => {
                let Some(path) = entry.on(Side::Right, left, right) else {
                    continue;
                };
                if mode.pushes(Side::Right) {
                    out.copies.push(Copy {
                        from: path.clone(),
                        to_dir: destination(&entry.path, left),
                    });
                } else if mode.removes(Side::Right) {
                    out.removals.push(path);
                }
            }
            Status::Different => {
                let Some(newer) = entry.newer else {
                    // 分不出新旧：动哪边都是赌，交给用户自己看。
                    out.skipped.push(Skip {
                        path: entry.path.clone(),
                        why: "两边都改过（时间分不出先后），不敢自动盖".to_string(),
                    });
                    continue;
                };
                if !mode.pushes(newer) {
                    continue;
                }
                let Some(from) = entry.on(newer, left, right) else {
                    continue;
                };
                let to_root = match newer {
                    Side::Left => right,
                    Side::Right => left,
                };
                out.copies.push(Copy {
                    from,
                    to_dir: destination(&entry.path, to_root),
                });
            }
        }
    }
    out
}

/// `a/b.txt` 拷到 `root` 下，该落在 `root/a/`。
fn destination(relative: &Path, root: &Path) -> PathBuf {
    match relative.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => root.join(parent),
        _ => root.to_path_buf(),
    }
}

/// 比较过程中送回来的东西。
#[derive(Clone, Debug)]
pub enum Event {
    /// 一批差异（边比边送）。
    Batch(Vec<Entry>),
    /// 比完了，连账一起。
    Done(Stats),
}

/// 起一个 OS 线程比一趟。返回的 [`Control`] 交给界面攥着。
pub fn spawn(options: Options, sink: Arc<dyn Fn(Event) + Send + Sync>) -> Control {
    analyze::spawn(sink, move |control, send| {
        let stats = compare(&options, control, &mut |batch| send(Event::Batch(batch)));
        send(Event::Done(stats));
    })
}

/// 把一个个拷贝按目标目录归堆：
/// 界面层的 `Ops` 一次搬一批到同一个目录，
/// 于是一个目录一个任务，进度、冲突、撤销都还是原来那一套。
pub fn group_copies(copies: Vec<Copy>) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for copy in copies {
        match groups.iter_mut().find(|(dir, _)| *dir == copy.to_dir) {
            Some((_, sources)) => sources.push(copy.from),
            None => groups.push((copy.to_dir, vec![copy.from])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// 结论句只报真发生的事：0 项不出现，"一样的"只在括号里报个数。
    #[test]
    fn summary_reports_only_what_happened() {
        let same = Stats {
            same: 12,
            ..Stats::default()
        };
        assert_eq!(same.summary(), "两边一样，共 12 项");

        let mixed = Stats {
            same: 4,
            different: 2,
            left_only: 1,
            right_only: 0,
            truncated: false,
            cancelled: false,
        };
        let text = mixed.summary();
        assert!(text.contains("不同 2 项"), "{text}");
        assert!(text.contains("只有左 1 项"), "{text}");
        assert!(!text.contains("只有右"), "0 项不许出现：{text}");
        assert!(text.contains("另有 4 项一样"), "{text}");

        let clipped = Stats {
            different: 5000,
            truncated: true,
            ..Stats::default()
        };
        assert!(
            clipped.summary().contains("已到上限"),
            "{}",
            clipped.summary()
        );
    }

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("建目录");
        }
        let mut file = fs::File::create(path).expect("建文件");
        file.write_all(contents.as_bytes()).expect("写文件");
        file.flush().expect("落盘");
    }

    /// 把修改时间挪到 `when`：比较靠 mtime 分先后，
    /// 而"写完就比"两边的 mtime 只差几毫秒，在容差之内分不出谁新。
    fn set_modified(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).expect("打开");
        file.set_modified(when).expect("改时间");
    }

    /// 让 `path` 明显比另一边新（容差是 2 秒，这里挪 10 秒）。
    fn make_newer(path: &Path) {
        set_modified(path, SystemTime::now() + Duration::from_secs(10));
    }

    fn pair() -> TempDir {
        let root = TempDir::new().expect("临时目录");
        fs::create_dir_all(root.path().join("l")).expect("建目录");
        fs::create_dir_all(root.path().join("r")).expect("建目录");
        root
    }

    fn run(options: &Options) -> (Vec<Entry>, Stats) {
        let mut entries = Vec::new();
        let stats = compare(options, &Control::new(), &mut |batch| entries.extend(batch));
        (entries, stats)
    }

    fn rel_paths(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|it| it.path.display().to_string())
            .collect()
    }

    #[test]
    fn identical_trees_have_no_diffs() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("a.txt"), "一样");
        write(right.join("a.txt"), "一样");
        let (entries, stats) = run(&Options::new(left, right));
        assert!(entries.is_empty(), "两边一样不该列出来：{entries:?}");
        assert_eq!(stats.same, 1);
        assert_eq!(stats.diffs(), 0);
    }

    #[test]
    fn a_file_only_on_one_side_is_reported() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("only.txt"), "只有左");
        write(right.join("sub/only.txt"), "只有右");
        let (entries, _) = run(&Options::new(left, right));
        assert_eq!(rel_paths(&entries), ["only.txt", "sub/only.txt"]);
        assert_eq!(entries[0].status, Status::LeftOnly);
        assert_eq!(entries[1].status, Status::RightOnly);
    }

    #[test]
    fn a_changed_file_says_which_side_is_newer() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("a.txt"), "老内容");
        write(right.join("a.txt"), "老内容");
        make_newer(&left.join("a.txt"));
        let (entries, stats) = run(&Options::new(left, right));
        assert_eq!(entries.len(), 1, "大小一样但时间不一样：{entries:?}");
        assert_eq!(entries[0].status, Status::Different);
        assert_eq!(entries[0].newer, Some(Side::Left));
        assert_eq!(stats.different, 1);
    }

    #[test]
    fn content_comparison_catches_same_size_and_same_time() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("a.bin"), "AAAA");
        write(right.join("a.bin"), "BBBB");
        let mut options = Options::new(left, right);
        // 时间可能碰巧一样，所以同时测两种口径：
        // 按元数据也可能判成不同（大小一样、时间不同），但按内容一定判成不同。
        options.by_content = true;
        let (entries, _) = run(&options);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Status::Different);
    }

    #[test]
    fn content_comparison_says_same_when_only_the_timestamp_differs() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("a.txt"), "一样的内容");
        write(right.join("a.txt"), "一样的内容");
        make_newer(&left.join("a.txt"));
        let mut options = Options::new(left, right);
        options.by_content = true;
        let (entries, stats) = run(&options);
        assert!(entries.is_empty(), "内容一样就不算差异：{entries:?}");
        assert_eq!(stats.same, 1);
    }

    #[test]
    fn noisy_directories_are_skipped_by_default() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("node_modules/pkg/a.js"), "左");
        write(right.join("node_modules/pkg/a.js"), "右");
        let (entries, _) = run(&Options::new(left, right));
        assert!(entries.is_empty(), "噪音目录不比：{entries:?}");
    }

    #[test]
    fn entries_come_out_in_a_stable_order() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        for name in ["c.txt", "a.txt", "b.txt"] {
            write(left.join(name), "只有左");
        }
        let (entries, _) = run(&Options::new(left, right));
        assert_eq!(rel_paths(&entries), ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn the_entry_limit_stops_the_listing_and_says_so() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        for index in 0..10 {
            write(left.join(format!("f{index}.txt")), "只有左");
        }
        let mut options = Options::new(left, right);
        options.limit = 3;
        let (entries, stats) = run(&options);
        assert_eq!(entries.len(), 3, "到上限就停");
        assert!(stats.truncated, "得如实说这是截断过的");
    }

    #[test]
    fn cancelling_stops_the_comparison() {
        let root = pair();
        let left = root.path().join("l");
        let right = root.path().join("r");
        write(left.join("a.txt"), "只有左");
        let control = Control::new();
        control.abort();
        let mut entries = Vec::new();
        let stats = compare(&Options::new(left, right), &control, &mut |batch| {
            entries.extend(batch)
        });
        assert!(entries.is_empty());
        assert!(stats.cancelled);
    }

    // ---- 同步计划 ----

    fn entry(path: &str, status: Status, newer: Option<Side>) -> Entry {
        let meta = Meta {
            bytes: 1,
            modified: None,
            digest: None,
        };
        Entry {
            path: PathBuf::from(path),
            status,
            newer,
            left: matches!(status, Status::LeftOnly | Status::Different).then(|| meta.clone()),
            right: matches!(status, Status::RightOnly | Status::Different).then(|| meta),
        }
    }

    #[test]
    fn one_way_copies_the_only_side_and_the_newer_side() {
        let left = PathBuf::from("/l");
        let right = PathBuf::from("/r");
        let entries = vec![
            entry("only-left.txt", Status::LeftOnly, Some(Side::Left)),
            entry("only-right.txt", Status::RightOnly, Some(Side::Right)),
            entry("sub/newer-left.txt", Status::Different, Some(Side::Left)),
            entry("newer-right.txt", Status::Different, Some(Side::Right)),
        ];
        let plan = plan(&entries, &left, &right, SyncMode::ToRight);
        let copied: Vec<String> = plan
            .copies
            .iter()
            .map(|it| format!("{} -> {}", it.from.display(), it.to_dir.display()))
            .collect();
        assert_eq!(
            copied,
            ["/l/only-left.txt -> /r", "/l/sub/newer-left.txt -> /r/sub",]
        );
        assert!(plan.removals.is_empty(), "单向不动对面多出来的");
    }

    #[test]
    fn both_ways_copies_each_newer_side_over() {
        let left = PathBuf::from("/l");
        let right = PathBuf::from("/r");
        let entries = vec![
            entry("a.txt", Status::Different, Some(Side::Left)),
            entry("b.txt", Status::Different, Some(Side::Right)),
            entry("c.txt", Status::LeftOnly, Some(Side::Left)),
        ];
        let plan = plan(&entries, &left, &right, SyncMode::BothWays);
        let copied: Vec<String> = plan
            .copies
            .iter()
            .map(|it| format!("{} -> {}", it.from.display(), it.to_dir.display()))
            .collect();
        assert_eq!(
            copied,
            ["/l/a.txt -> /r", "/r/b.txt -> /l", "/l/c.txt -> /r"]
        );
    }

    #[test]
    fn a_file_changed_on_both_sides_is_skipped_not_guessed() {
        let left = PathBuf::from("/l");
        let right = PathBuf::from("/r");
        let entries = vec![entry("both.txt", Status::Different, None)];
        let plan = plan(&entries, &left, &right, SyncMode::BothWays);
        assert!(plan.copies.is_empty(), "分不出先后就不该动：{plan:?}");
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].why.contains("不敢自动盖"));
    }

    #[test]
    fn mirroring_removes_the_extra_files_on_the_other_side() {
        let left = PathBuf::from("/l");
        let right = PathBuf::from("/r");
        let entries = vec![
            entry("only-left.txt", Status::LeftOnly, Some(Side::Left)),
            entry("only-right.txt", Status::RightOnly, Some(Side::Right)),
        ];
        let plan = plan(&entries, &left, &right, SyncMode::MirrorToRight);
        assert_eq!(plan.copies.len(), 1, "左边独有的拷过去");
        assert_eq!(plan.removals, [PathBuf::from("/r/only-right.txt")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn copies_are_grouped_by_destination_directory() {
        let copies = vec![
            Copy {
                from: PathBuf::from("/l/a.txt"),
                to_dir: PathBuf::from("/r"),
            },
            Copy {
                from: PathBuf::from("/l/sub/b.txt"),
                to_dir: PathBuf::from("/r/sub"),
            },
            Copy {
                from: PathBuf::from("/l/c.txt"),
                to_dir: PathBuf::from("/r"),
            },
        ];
        let grouped = group_copies(copies);
        assert_eq!(grouped.len(), 2, "同目录的归成一堆");
        assert_eq!(
            grouped[0].1,
            [PathBuf::from("/l/a.txt"), PathBuf::from("/l/c.txt")]
        );
        assert_eq!(grouped[1].0, PathBuf::from("/r/sub"));
    }

    #[test]
    fn an_empty_plan_says_it_is_empty() {
        assert!(Plan::default().is_empty());
    }
}
