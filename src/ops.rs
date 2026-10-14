//! 文件操作：计划、执行、冲突处理与撤销。
//!
//! 这一层不依赖 UI：它只回答“做什么、按什么顺序做、做错了怎么退回”。
//! 所有函数都是同步的，由调用方放到后台线程执行，通过 [`Progress`] 回调汇报进度。

use std::{
    collections::HashMap,
    fs,
    io::{self, Read as _, Seek as _, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering::SeqCst},
    },
    time::{Duration, SystemTime},
};

use crate::remote::{self, Client};
use crate::resume;

/// 目标已存在时的处理方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// 双方都留下：自动改个不冲突的名字。
    #[default]
    KeepBoth,
    /// 用源替换目标。
    Replace,
    /// 不动目标。
    Skip,
}

/// 一次操作的进度快照，界面据此画进度条、算速度和剩余时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// 当前动作的人类可读描述，例如“正在拷贝 report.pdf”。
    pub label: String,
    /// 已经处理完的条目数。
    pub done: usize,
    /// 条目总数。
    pub total: usize,
    /// 已经写过去的字节数。删除、重命名这类不按字节算的操作留 0。
    pub bytes_done: u64,
    /// 这次总共要搬多少字节。数不出来（没权限）就是 0，
    /// 界面退化成只看条目数，不要拿 0 去除。
    pub bytes_total: u64,
}

/// 一个正在跑的任务的控制手柄。
///
/// UI 拿一份、工作线程拿一份（两个句柄指向同一份状态），
/// 所以"暂停"和"中止"不需要任何回调或消息往返——
/// 工作线程在每个数据块之间自己问一次就行。
#[derive(Clone, Default)]
pub struct Control(Arc<AtomicU8>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlState {
    Running,
    Paused,
    Aborting,
}

impl Control {
    const RUNNING: u8 = 0;
    const PAUSED: u8 = 1;
    const ABORTING: u8 = 2;

    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(Self::RUNNING)))
    }

    pub fn pause(&self) {
        self.0.store(Self::PAUSED, SeqCst);
    }

    /// 继续。中止是不可撤销的，所以只在暂停态生效。
    pub fn resume(&self) {
        self.0
            .compare_exchange(Self::PAUSED, Self::RUNNING, SeqCst, SeqCst)
            .ok();
    }

    pub fn abort(&self) {
        self.0.store(Self::ABORTING, SeqCst);
    }

    pub fn state(&self) -> ControlState {
        match self.0.load(SeqCst) {
            Self::PAUSED => ControlState::Paused,
            Self::ABORTING => ControlState::Aborting,
            _ => ControlState::Running,
        }
    }

    /// 工作线程在每个数据块之间问一次：暂停时在这里等到恢复，
    /// 要中止就返回 false。
    ///
    /// 睡 50 毫秒再问，不用条件变量：这个循环只在暂停期间转，
    /// 而暂停本来就是人在等，50 毫秒的粒度绰绰有余。
    pub fn should_continue(&self) -> bool {
        loop {
            match self.state() {
                ControlState::Running => return true,
                ControlState::Aborting => return false,
                ControlState::Paused => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// 一次操作的结果：成功多少、跳过多少、哪些失败了。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub done: usize,
    pub skipped: usize,
    pub failed: Vec<(PathBuf, crate::i18n::Msg)>,
    /// 本次操作实际创建或移动出来的路径，界面刷新用它。
    pub touched: Vec<PathBuf>,
    /// 移动操作的“源 -> 目标”配对，撤销移动要用。
    pub transfers: Vec<(PathBuf, PathBuf)>,
}

// ---------------------------------------------------------------------------
// 命名与冲突
// ---------------------------------------------------------------------------

/// 在 `parent` 下为“未命名文件夹”找一个没人用的名字。
pub fn fresh_folder_path(parent: &Path) -> PathBuf {
    let mut candidate = parent.join("未命名文件夹");
    let mut round = 2;
    while candidate.exists() {
        candidate = parent.join(format!("未命名文件夹 {round}"));
        round += 1;
    }
    candidate
}

/// 拆出主干和扩展名，用于拼“主干 2.扩展名”。
fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        // 点号在开头（“.gitignore”）不算扩展名。
        Some(ix) if ix > 0 => (name[..ix].to_string(), name[ix..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// 给已存在的目标找一个不冲突的兄弟路径：`a.txt` → `a 2.txt` → `a 3.txt`。
pub fn unique_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "副本".to_string());
    let (stem, extension) = split_name(&name);

    let mut round = 2;
    loop {
        let candidate = target.with_file_name(format!("{stem} {round}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
        round += 1;
    }
}

/// 目标已存在时，按策略决定最终落在哪里；`None` 表示跳过。
pub fn resolve_conflict(target: &Path, policy: ConflictPolicy) -> Option<PathBuf> {
    match policy {
        ConflictPolicy::KeepBoth => Some(unique_sibling(target)),
        ConflictPolicy::Replace => Some(target.to_path_buf()),
        ConflictPolicy::Skip => None,
    }
}

/// 同一个文件的两种写法（大小写不同、或相对/绝对路径不同）在部分文件系统上
/// 会同时 `exists()`，重命名前要先放行这种情况。
fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 单项操作
// ---------------------------------------------------------------------------

/// 用指定的名字新建文件夹；名字已存在时报错，由界面留在输入框里让用户改。
pub fn create_folder_named(parent: &Path, name: &str) -> Result<PathBuf, crate::i18n::Msg> {
    valid_name(name)?;
    let trimmed = name.trim();
    let path = parent.join(trimmed);
    if path.exists() {
        return Err(crate::i18n::Message::args(
            "“{trimmed}”已存在",
            &[("trimmed", trimmed.to_string())],
        ));
    }
    fs::create_dir(&path)
        .map_err(|error| crate::i18n::Message::caused("无法创建文件夹：{error}", error))?;
    Ok(path)
}

/// 重命名。只改名字，不换目录。
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, crate::i18n::Msg> {
    valid_name(new_name)?;
    let trimmed = new_name.trim();

    let parent = path
        .parent()
        .ok_or_else(|| crate::i18n::Message::msg("没有父目录，无法重命名"))?;
    let target = parent.join(trimmed);
    if target.exists() && !same_file(path, &target) {
        return Err(crate::i18n::Message::args(
            "“{trimmed}”已存在",
            &[("trimmed", trimmed.to_string())],
        ));
    }

    fs::rename(path, &target)
        .map_err(|error| crate::i18n::Message::caused("重命名失败：{error}", error))?;
    Ok(target)
}

/// 移进回收站。返回 `(原路径, 删除时刻)` 列表，撤销时用。
///
/// 删除时刻是撤销的关键依据：回收站里的同名项可能不止一个
/// （删过一次又删一次），只有“这次删的”才该被放回来。
pub fn trash(paths: &[PathBuf]) -> Vec<(PathBuf, SystemTime)> {
    let trashed_at = SystemTime::now();
    let mut trashed = Vec::new();
    for path in paths {
        if trash::delete(path).is_ok() {
            trashed.push((path.clone(), trashed_at));
        }
    }
    trashed
}

/// 从回收站放回原处。
///
/// - Linux/Windows：`trash` crate 的 `os_limited` 能直接列出并放回，可靠。
/// - macOS：系统不提供查询接口，改为扫描 `~/.Trash` 里同名、且 inode 变更时间
///   （ctime，移入回收站会刷新）不早于删除时刻的项，取最新的一个放回。
///   这是启发式：万一失败会给出明确错误，让用户去回收站手动放回。
pub fn restore_from_trash(original: &Path, trashed_at: SystemTime) -> Result<(), crate::i18n::Msg> {
    #[cfg(any(
        target_os = "windows",
        all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        )
    ))]
    {
        let items = trash::os_limited::list().map_err(crate::i18n::Message::from)?;
        let mut matches: Vec<_> = items
            .into_iter()
            .filter(|item| item.original_path() == original)
            .collect();
        // 时间戳越大删除越晚，放回最新的那条。
        matches.sort_by_key(|item| item.time_deleted);
        let Some(item) = matches.pop() else {
            return Err(crate::i18n::Message::msg("回收站里找不到这一项"));
        };
        return trash::os_limited::restore_all([item]).map_err(|error| error.to_string());
    }

    #[cfg(target_os = "macos")]
    {
        let trash_dir = dirs::home_dir()
            .ok_or_else(|| crate::i18n::Message::msg("找不到个人文件夹，无法定位回收站"))?
            .join(".Trash");
        restore_from_trash_dir(&trash_dir, original, trashed_at)
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        all(unix, not(target_os = "ios"), not(target_os = "android"))
    )))]
    {
        let _ = (original, trashed_at);
        Err(crate::i18n::Message::msg(
            "这个平台不支持从回收站撤销，请手动打开回收站放回",
        ))
    }
}

/// macOS 的回收站恢复实现，回收站目录由调用方给出（测试里用临时目录）。
#[cfg(target_os = "macos")]
fn restore_from_trash_dir(
    trash_dir: &Path,
    original: &Path,
    trashed_at: SystemTime,
) -> Result<(), crate::i18n::Msg> {
    use std::os::unix::fs::MetadataExt as _;

    let name = original
        .file_name()
        .ok_or_else(|| crate::i18n::Message::msg("无法解析原文件名"))?
        .to_string_lossy()
        .into_owned();
    let (stem, extension) = split_name(&name);

    // 留 2 秒余量，吸收“删除动作先记时刻、系统后落盘”的时钟误差。
    let cutoff = trashed_at - Duration::from_secs(2);
    let mut best: Option<(i64, PathBuf)> = None;

    let entries = fs::read_dir(trash_dir).map_err(crate::i18n::Message::from)?;
    for entry in entries.flatten() {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        if !trash_name_matches(&entry_name, &stem, &extension) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        // 移入回收站会刷新 inode 的 ctime；早于删除时刻的是更早删的，不动。
        if metadata.ctime() < cutoff_seconds(cutoff) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(seen, _)| metadata.ctime() > *seen)
        {
            best = Some((metadata.ctime(), entry.path()));
        }
    }

    let (_, candidate) = best.ok_or_else(|| {
        crate::i18n::Message::msg("回收站里找不到这次删掉的项，请手动打开回收站放回")
    })?;

    if original.exists() {
        return Err(crate::i18n::Message::msg(
            "原位置已经有同名项目，先移走它再撤销",
        ));
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::rename(&candidate, original).map_err(crate::i18n::Message::from)
}

/// 回收站里的名字是“原名”或“原名 N”（macOS 遇到同名项时自动加序号）。
#[cfg(target_os = "macos")]
fn trash_name_matches(entry_name: &str, stem: &str, extension: &str) -> bool {
    if entry_name == format!("{stem}{extension}") {
        return true;
    }
    let Some(rest) = entry_name.strip_prefix(stem) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(extension) else {
        return false;
    };
    // 名字和扩展名之间只允许一个空格加数字，例如 " 2"。
    match rest.strip_prefix(' ') {
        Some(digits) => !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

#[cfg(target_os = "macos")]
fn cutoff_seconds(cutoff: SystemTime) -> i64 {
    cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 批量拷贝 / 移动
// ---------------------------------------------------------------------------

/// 递归拷贝一个条目（文件或目录）到 `dest`。
/// 一次读写的数据块大小。
const COPY_CHUNK: usize = 1024 * 1024;

fn copy_entry(
    source: &Path,
    dest: &Path,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), crate::i18n::Msg> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dest.join(entry.file_name()), report)?;
        }
        Ok(())
    } else {
        copy_file(source, dest, report)
    }
}

/// 拷贝单个文件，每拷完一块上报一次字节数。
///
/// 不用 `fs::copy`：它没法报进度，也就没法在拷贝大文件的中途暂停或显示速度。
/// 代价是放弃了 macOS APFS 上 clonefile 那种"瞬间复制"的优化——
/// 一个能暂停、能显示剩余时间的操作中心比这个优化值钱：
/// 用户暂停之后等三分钟才真的停下来，等于没有暂停。
///
/// 也**不往目标上直接写**：先写状态目录底下的暂存文件，成了才落成目标。
/// 这样中止一个 40 GB 的拷贝，目标目录里不会多一个看着像传好了的半截货，
/// 而半截数据还留在暂存处——下一次同样的 (源, 目标) 会接着传（见 `resume`）。
///
/// **返回 `Msg` 而不是 `io::Result`**：整条拷贝链上凡是自己的措辞
/// （「落不了盘」「源和目标在同一个文件上」）都要能翻译，而中途冒出来的
/// IO 错误靠 `?` 自动转成 `verbatim` 透传——「Permission denied」
/// 是操作系统的话，翻了它用户反而认不出这是谁讲的。
fn copy_file(
    source: &Path,
    target: &Path,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), crate::i18n::Msg> {
    // 拷到自己身上会把源文件先截断成 0 字节——`fs::copy` 内置了这个检查，
    // 手写拷贝必须自己补上。
    if same_file(source, target) {
        return Err(crate::i18n::Message::msg("源和目标在同一个文件上"));
    }

    let journal = resume::journal();
    let key = resume::key(&show(source), &show(target));
    // 从这一刻起（暂存文件建出来之前）就把这一对占住，
    // 免得并发的另一趟搬运顺手 sweep() 把正在写的半截数据当垃圾删掉。
    let _hold = journal.hold(&key);
    let metadata = fs::metadata(source)?;
    let total = metadata.len();
    let modified = modified_of(&metadata);
    // 上一次传到哪儿了。源对不上（大小或时间变了）这本账就作废，
    // 老老实实从头传——接着一个已经变了的文件传，得到的是个四不像。
    let mut done = match journal.load(&key) {
        Some(record) if record.fresh(total, modified) => record.done,
        _ => {
            journal.clear(&key);
            0
        }
    };

    let staging = journal.staging(&key);
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent)?;
    }
    // 暂存文件可能被清掉过（系统清临时目录、用户手动清）：磁盘上没那么多
    // 字节就别逞强，从实际有的地方接。
    let on_disk = fs::metadata(&staging).map(|it| it.len()).unwrap_or(0);
    if on_disk < done {
        done = on_disk;
        if done == 0 {
            journal.clear(&key);
        }
    }

    let mut reader = fs::File::open(source)?;
    // **不开 `truncate`**：一截就回到 0，续传直接失效。
    // 要多长由下面那句 `set_len(done)` 说了算。
    #[allow(clippy::suspicious_open_options)]
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&staging)?;
    // 不管原来有多长，就认前 `done` 个字节：多出来的当垃圾截掉，
    // 然后**读写都从断点起**。少任何一半都会出事——
    // 只 seek 读不 seek 写，接出来的文件是错位的；
    // 不截断，暂存文件会一直带着上次的尾巴长大。
    writer.set_len(done)?;
    if done > 0 {
        reader.seek(io::SeekFrom::Start(done))?;
        writer.seek(io::SeekFrom::Start(done))?;
    }

    // 先把已经传好的那部分报上去：进度条从 45% 开始，
    // 而不是从 0 猛跳到 45%（那看起来像卡了一下）。
    if done > 0 && !report(done) {
        journal.save(
            &key,
            &resume::Record {
                source: show(source),
                size: total,
                modified,
                done,
            },
        );
        journal.sweep();
        return Err(aborted_msg());
    }

    let result = pump(&mut reader, &mut writer, &mut done, report);
    writer.flush().ok();
    // 先放手：Windows 上握着打开的文件，改名/删除都会失败。
    drop(writer);

    match result {
        Ok(()) => {
            // 暂存文件还在就改名落成目标；已经叫别人清掉了（系统清临时目录，
            // 或者哪个测试把自己的状态目录连窝端了）就换个法子把内容补上。
            land(&staging, target, || {
                fs::copy(source, target).map(|_| ()).map_err(|error| {
                    crate::i18n::Message::args("落不了盘：{error}", &[("error", error.to_string())])
                })
            })?;
            // `fs::copy` 会带上权限位，手写拷贝必须自己补：
            // 否则拷出来的脚本和程序会丢掉执行权限。
            if let Ok(metadata) = fs::metadata(source) {
                fs::set_permissions(target, metadata.permissions()).ok();
            }
            journal.clear(&key);
            Ok(())
        }
        Err(error) => {
            // 只把进度记下来：暂存文件留着，目标上一个字节都没碰。
            // 下一次同样的 (源, 目标) 会从 `done` 接着来。
            journal.save(
                &key,
                &resume::Record {
                    source: show(source),
                    size: total,
                    modified,
                    done,
                },
            );
            // 暂存变大了：顺手扫一遍，别让它悄悄把磁盘吃干。
            journal.sweep();
            Err(error)
        }
    }
}

/// 读一块、写一块、报一块。`done` 是已经落盘的字节数，跟着一起往前走，
/// 好让调用方知道该把断点记在哪儿。
fn pump(
    reader: &mut fs::File,
    writer: &mut fs::File,
    done: &mut u64,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), crate::i18n::Msg> {
    let mut buffer = vec![0u8; COPY_CHUNK];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..read])?;
        *done += read as u64;
        if !report(read as u64) {
            // 只删这一个没拷完的文件，由**这一层**负责：
            // 拷文件夹时兄弟们已经拷好了，在外面删会把整棵子树连坐删掉——
            // 现在更简单了：半截的在暂存处，目标里本来就没有它。
            return Err(aborted_msg());
        }
    }
}

/// 路径转成字符串，只为存进续传账本（给人看、也给同一个键用）。
fn show(path: &Path) -> String {
    path.display().to_string()
}

/// 用户按了中止时抛出的错误。
///
/// **为什么不能用 `ErrorKind::Interrupted`**：那个种类的含义是"被信号
/// 打断，重试一次就好"，于是 `write_all` / `read_exact` / `io::copy`
/// 都会把它当成该重试的错误——中止一个下载会变成无限重试，
/// 一直写到磁盘满才停下（实测写掉了 28 GB）。
/// 换成一个不触发重试的种类，再靠 [`crate::i18n::Aborted`] 这个类型
/// 认出它（不是靠文案——文案能改、能翻，判据不能跟着变）。
fn aborted() -> io::Error {
    io::Error::other(crate::i18n::Aborted)
}

/// 用户中止了，拷贝链上用的那一份（[`copy_file`] 走 `Msg`）。
///
/// 同一个意思两个类型：进 [`Reporting`] 的必须还是 `io::Error`——
/// 它实现的是 `std::io::Read`，签名改不了。
fn aborted_msg() -> crate::i18n::Msg {
    crate::i18n::Message::aborted()
}

/// 这个错误是不是"用户中止了"。
///
/// **判 key 不判显示文案**（[`crate::i18n::Message::is_aborted`]）：
/// 拿 `to_string()` 判在中文下一直成立，切到英文就永远不成立——那一趟
/// 会被当成真失败，白扫一遍暂存。
fn is_aborted(error: &crate::i18n::Msg) -> bool {
    error.is_aborted()
}

/// 修改时间（Unix 秒）。取不到就没有——续传账本拿它当"源没变过"的证据之一。
/// 修改时间（Unix 纳秒）。取不到就没有——续传账本拿它当"源没变过"的证据之一。
///
/// **为什么精确到纳秒而不是秒**：秒级精度下，"拷到一半停下来、同一秒内
/// 又改了源文件、再接着传"这种情形会被判成"源没变过"，于是接着旧断点
/// 往下拼，得到一个前半截是旧内容、后半截是新内容的四不像。
/// APFS / ext4 / NTFS 都能给出亚秒精度；碰到只有秒级的文件系统
/// （老 HFS+、FAT），精度自然退化，不会比现在更糟。
fn modified_of(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|it| it.as_nanos() as u64)
}

/// 递归删除，目录用完了才能这么干；调用方需确认过。
fn remove_entry(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// 把一组条目拷进 `dest_dir`。
///
/// 冲突按 `policy` 统一处理（界面先问一次，不再逐项打断）。
/// `on_progress` 每完成一项、以及每拷完一个数据块都调用一次；
/// 返回 false 表示用户要求中止（暂停由调用方在它内部等，worker 不必知道）。
///
/// `remote` 两端都在本地时传 [`RemoteSides::default()`]。
pub fn copy_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    remote: &RemoteSides,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    transfer_entries(sources, dest_dir, policy, false, remote, on_progress)
}

/// 把一组条目移进 `dest_dir`；跨卷时按“复制成功后删源”处理。
pub fn move_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    remote: &RemoteSides,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    transfer_entries(sources, dest_dir, policy, true, remote, on_progress)
}

fn transfer_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    remove_source: bool,
    remote: &RemoteSides,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    let total = sources.len();
    // 总字节数在后台线程里数：这一趟本来就要遍历一遍，顺手就出来了。
    let bytes_total = count_bytes(sources, remote.source.as_deref());
    let mut outcome = Outcome::default();
    let mut bytes_done = 0u64;

    for (ix, source) in sources.iter().enumerate() {
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.display().to_string());
        let label = if remove_source {
            format!("正在移动 {name}")
        } else {
            format!("正在拷贝 {name}")
        };
        if !on_progress(&Progress {
            label: label.clone(),
            done: ix,
            total,
            bytes_done,
            bytes_total,
        }) {
            outcome.skipped += total - ix;
            return outcome;
        }

        let target = dest_dir.join(&name);
        // 往自己的父目录移动自己是无意义的，也不该判成冲突。
        let same_dir = source.parent() == Some(dest_dir);
        let exists = match remote.dest.as_deref() {
            Some(client) => remote_exists(client, &target),
            None => target.exists(),
        };
        let conflict = exists && !same_file(source, &target) && !same_dir;

        let Some(target) = (if conflict {
            resolve_target(&target, policy, remote.dest.as_deref())
        } else {
            Some(target)
        }) else {
            outcome.skipped += 1;
            continue;
        };

        // 目标是移动源的子目录时会形成环，直接拒绝。
        if remove_source && target.starts_with(source) {
            outcome.failed.push((
                source.clone(),
                crate::i18n::Message::msg("不能把文件夹移进它自己"),
            ));
            continue;
        }

        // 把上报器关进一个块里：它借走了 `on_progress`，
        // 不先还回来下一轮的进度就发不出去了。
        let result = {
            let mut report = |bytes: u64| {
                bytes_done += bytes;
                on_progress(&Progress {
                    label: label.clone(),
                    done: ix,
                    total,
                    bytes_done,
                    bytes_total,
                })
            };
            transfer_one(source, &target, remove_source, remote, &mut report)
        };

        match result {
            Ok(()) => {
                outcome.done += 1;
                outcome.touched.push(target.clone());
                if remove_source {
                    outcome.transfers.push((source.clone(), target));
                }
            }
            Err(TransferError::Aborted) => {
                outcome.skipped += total - ix;
                return outcome;
            }
            Err(TransferError::Failed(message)) => {
                outcome.failed.push((source.clone(), message));
            }
        }
    }

    on_progress(&Progress {
        label: "完成".to_string(),
        done: total,
        total,
        bytes_done,
        bytes_total,
    });
    outcome
}

/// 一次搬运两端的远程客户端；两端都在本地时两个都是 `None`。
///
/// 客户端必须**在主线程上取好再搬进后台线程**：`Remotes` 是 GPUI 实体，
/// 后台线程里读不到它。取好的客户端是同步的，跨线程带着走没问题。
#[derive(Clone, Default)]
pub struct RemoteSides {
    /// 源在远程时的客户端（查、下载、移动成功后删源）。
    pub source: Option<Client>,
    /// 目标在远程时的客户端（判重、建目录、上传）。
    pub dest: Option<Client>,
}

/// `dav://<连接 id>/<子路径>` → 子路径。
fn sub_of(path: &Path) -> Result<String, TransferError> {
    remote::parse_location(path)
        .map(|(_, sub)| sub)
        .ok_or_else(|| {
            TransferError::Failed(crate::i18n::Message::args(
                "不是远程路径：{path}",
                &[("path", path.display().to_string())],
            ))
        })
}

/// 远程目标上有没有这一项。`Path::exists()` 对 `dav://…` 永远是假，
/// 判冲突必须问服务器。
pub fn remote_exists(client: &dyn remote::Remote, target: &Path) -> bool {
    let Some((_, sub)) = remote::parse_location(target) else {
        return false;
    };
    client.stat(&sub).unwrap_or(None).is_some()
}

/// 目标已存在时，按策略挑最终落点；`None` = 跳过这一项。
///
/// 本地那一支走 [`resolve_conflict`]（靠文件系统判"有没有"），
/// 远程这一支改成问服务器，并且"保留两者"的候选名也得由服务器确认没人用。
fn resolve_target(
    target: &Path,
    policy: ConflictPolicy,
    client: Option<&dyn remote::Remote>,
) -> Option<PathBuf> {
    let Some(client) = client else {
        return resolve_conflict(target, policy);
    };
    let Some((id, sub)) = remote::parse_location(target) else {
        return resolve_conflict(target, policy);
    };
    match policy {
        ConflictPolicy::Skip => None,
        ConflictPolicy::Replace => Some(target.to_path_buf()),
        ConflictPolicy::KeepBoth => Some(remote::location(&id, &unique_remote(client, &sub))),
    }
}

/// 在远程那一端给 `sub` 找一个没人用的兄弟：`a.txt` → `a 2.txt`。
fn unique_remote(client: &dyn remote::Remote, sub: &str) -> String {
    let (prefix, name) = match sub.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), sub.to_string()),
    };
    let (stem, extension) = match name.rfind('.') {
        Some(at) if at > 0 => (name[..at].to_string(), name[at..].to_string()),
        _ => (name, String::new()),
    };
    let mut round = 2;
    loop {
        let candidate = format!("{prefix}{stem} {round}{extension}");
        if client.stat(&candidate).unwrap_or(None).is_none() {
            return candidate;
        }
        round += 1;
    }
}

/// 搬运单项的两种结局。
///
/// 分开是为了让"用户中止"不会变成一条失败记录——
/// 操作中心会把失败项列出来让人重试，被中止的不该混进去。
pub enum TransferError {
    Aborted,
    Failed(crate::i18n::Msg),
}

fn transfer_one(
    source: &Path,
    target: &Path,
    remove_source: bool,
    remote: &RemoteSides,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), TransferError> {
    match (remote.source.as_deref(), remote.dest.as_deref()) {
        (None, None) => local_to_local(source, target, remove_source, report),
        // 远程 -> 本地：下载。移动时下载成功后删掉服务器上的那一份。
        (Some(client), None) => {
            let sub = sub_of(source)?;
            download_entry(client, &sub, target, report)?;
            if remove_source {
                client.remove(&sub).map_err(|error| {
                    TransferError::Failed(crate::i18n::Message::args(
                        "已下载但删除远程源失败：{error}",
                        &[("error", error.to_string())],
                    ))
                })?;
            }
            Ok(())
        }
        // 本地 -> 远程：上传。
        (None, Some(client)) => {
            let sub = sub_of(target)?;
            upload_entry(client, source, &sub, remove_source, report)
        }
        // 远程 -> 远程：同一台服务器就让服务器自己拷，一趟 MOVE/COPY 就完事；
        // 跨服务器要先下载再上传，而"下载到哪儿"说不清（也没有可见的落点），
        // 所以明确不支持，好过偷偷在临时目录里绕一圈。
        (Some(from), Some(to)) => {
            if from.id() != to.id() {
                return Err(TransferError::Failed(crate::i18n::Message::msg(
                    "不支持在两个不同的服务器之间直接搬运",
                )));
            }
            let from_sub = sub_of(source)?;
            let to_sub = sub_of(target)?;
            if remove_source {
                from.rename(&from_sub, &to_sub)
            } else {
                from.copy(&from_sub, &to_sub)
            }
            .map_err(TransferError::Failed)
        }
    }
}

fn local_to_local(
    source: &Path,
    target: &Path,
    remove_source: bool,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), TransferError> {
    if remove_source {
        // 同卷移动一步到位；跨卷先拷贝再删源。
        if fs::rename(source, target).is_ok() {
            return Ok(());
        }
    }

    // 拷到一半停下时，没拷完的那一个留在续传暂存处，目标里本来就没有它，
    // 所以这里不用、也不能删 `target`：拷目录时 `target` 是整棵子树，
    // 里面有已经拷好的东西。
    copy_entry(source, target, report).map_err(|error| {
        if is_aborted(&error) {
            TransferError::Aborted
        } else {
            TransferError::Failed(error)
        }
    })?;

    if remove_source {
        remove_entry(source).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "已拷贝但删除源文件失败：{error}",
                &[("error", error.to_string())],
            ))
        })?;
    }
    Ok(())
}

/// 从远程下载一项（文件或整棵子树）到本地 `target`。
fn download_entry(
    client: &dyn remote::Remote,
    sub: &str,
    target: &Path,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), TransferError> {
    let info = client
        .stat(sub)
        .map_err(TransferError::Failed)?
        .ok_or_else(|| TransferError::Failed(crate::i18n::Message::msg("远程那一项已经不在了")))?;

    if info.is_dir {
        fs::create_dir_all(target).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "建不了本地目录：{error}",
                &[("error", error.to_string())],
            ))
        })?;
        let children = client.list(sub).map_err(TransferError::Failed)?;
        for child in children {
            download_entry(client, &child.path, &target.join(&child.name), report)?;
        }
        return Ok(());
    }

    // 下载也走暂存 + 续传：远程断了线，下一次从服务器上半路接着要，
    // 而用户的目录里始终只有完整的文件。
    let journal = resume::journal();
    let key = resume::key(&remote_source(client, sub), &show(target));
    // 同上：占住，别让并发的清扫动正在下载的这一块。
    let _hold = journal.hold(&key);
    let can_resume = client.resumes_download();
    let mut done = match journal.load(&key) {
        Some(record) if can_resume && record.fresh(info.size, info.modified) => record.done,
        _ => {
            journal.clear(&key);
            0
        }
    };
    let staging = journal.staging(&key);
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "建不了暂存目录：{error}",
                &[("error", error.to_string())],
            ))
        })?;
    }
    // 暂存文件被动过（系统清临时目录、磁盘满写坏了）：以磁盘上实际有的为准。
    let on_disk = fs::metadata(&staging).map(|it| it.len()).unwrap_or(0);
    if on_disk < done {
        done = on_disk;
        if done == 0 {
            journal.clear(&key);
        }
    }

    // 同上：不开 `truncate`，长度由紧接着的 `set_len(done)` 定。
    #[allow(clippy::suspicious_open_options)]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&staging)
        .map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "写不了本地文件：{error}",
                &[("error", error.to_string())],
            ))
        })?;
    // 同上：只认前 `done` 个字节，读写都从断点起。
    file.set_len(done).map_err(|error| {
        TransferError::Failed(crate::i18n::Message::args(
            "写不了本地文件：{error}",
            &[("error", error.to_string())],
        ))
    })?;
    if done > 0 {
        file.seek(io::SeekFrom::Start(done)).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "写不了本地文件：{error}",
                &[("error", error.to_string())],
            ))
        })?;
    }

    // 续上的那部分先报一次，进度条从半路开始。
    if done > 0 && !report(done) {
        drop(file);
        note_download(&journal, &key, client, sub, &info, done);
        journal.sweep();
        return Err(TransferError::Aborted);
    }

    let (result, was_aborted) = {
        let mut sink = Reporting::new(&mut file, report);
        let result = if done > 0 {
            client.download_from(sub, done, &mut sink)
        } else {
            client.download(sub, &mut sink)
        };
        // 顺序要紧：先问 `sink` 要「用户按过中止没有」，再让它掉出作用域。
        // 问错了地方就只能回去比错误文案——而 `ureq` 包装过的那句已经
        // 认不出来了。理由见 `Reporting`。
        (result, sink.aborted())
    };
    // 先关掉句柄：Windows 上开着的文件动不了。
    drop(file);
    match result {
        Ok(_) => {
            land(&staging, target, || {
                // 暂存叫清了：重新下一趟，直接落到目标上。
                // 不报进度——这些字节前面已经报过了，再报一遍进度条会跑过 100%。
                let mut file = fs::File::create(target).map_err(|error| {
                    crate::i18n::Message::caused("写不了本地文件：{error}", error)
                })?;
                client.download(sub, &mut file)?;
                Ok(())
            })
            .map_err(TransferError::Failed)?;
            journal.clear(&key);
            Ok(())
        }
        Err(error) => {
            // 半截数据留着：这是下一次的起点。目标里一个字节都没有，
            // 所以"失败"之后用户的目录是干净的，只是还没拿到文件。
            let done = fs::metadata(&staging).map(|it| it.len()).unwrap_or(done);
            // 但有一种"中止"其实下完了：`Reporting` 是读完一块才报进度，
            // 所以那块恰好覆盖到文件末尾时，回调说停就停，暂存里躺着的
            // 是一份完整的文件。这种时候再判中止等于白下一趟——用户已经
            // 有一份完整文件躺在暂存里，却得看着它被扫掉重来。
            // 判据只能是长度：续传账本拿不到"服务器那边还剩多少"。
            if was_aborted && done >= info.size {
                land(&staging, target, || Ok(())).map_err(TransferError::Failed)?;
                journal.clear(&key);
                return Ok(());
            }
            note_download(&journal, &key, client, sub, &info, done);
            journal.sweep();
            if was_aborted {
                Err(TransferError::Aborted)
            } else {
                Err(TransferError::Failed(error))
            }
        }
    }
}

/// 下载的续传账本怎么记：`done` 以暂存文件实际有多长为准。
fn note_download(
    journal: &resume::Journal,
    key: &str,
    client: &dyn remote::Remote,
    sub: &str,
    info: &remote::Entry,
    done: u64,
) {
    journal.save(
        key,
        &resume::Record {
            source: remote_source(client, sub),
            size: info.size,
            modified: info.modified,
            done,
        },
    );
}

/// 远程那一项在账本里的名字。
///
/// 带 `remote://` 前缀是为了跟本地路径分得开：本地路径也可能叫 `dav/x.txt`，
/// 而两本账撞键的后果是"接着一个完全不相干的目标传"。
fn remote_source(client: &dyn remote::Remote, sub: &str) -> String {
    format!("remote://{}/{}", client.id(), sub)
}

/// 把暂存文件落成目标；暂存已经不见了就换个法子把内容补上。
///
/// `fallback` 是"没有暂存文件时怎么把内容搬过去"：本地拷贝是整个重拷一遍，
/// 远程下载是重新下一趟。两条路都**不报进度**——这些字节前面已经报过了，
/// 再报一遍进度条就会跑过 100%。
///
/// **收 `Msg` 而不是 `io::Result`**：这三条落地路（改名、拷贝、下载）最后都要
/// 冒泡成界面上一行话，各自都有自己的措辞。让它们在各自的位置攒 `Msg`，
/// 比在这里降级成 `io::Error` 再在外层重新包一次省事，也不会把「落不了盘」
/// 这种自己的措辞漏进 `io::Error`（漏进去就再也翻译不了了）。
fn land(
    staging: &Path,
    target: &Path,
    fallback: impl FnOnce() -> Result<(), crate::i18n::Msg>,
) -> Result<(), crate::i18n::Msg> {
    if staging.exists() {
        resume::finish(staging, target)
    } else {
        fallback()
    }
}

/// 把本地一项（文件或整棵子树）上传到远程 `sub`。
///
/// `remove_source` 为真时，上传成功后删掉本地那份——这是"剪切"，
/// 也用于把一次远程 -> 本地的移动撤销回去。
fn upload_entry(
    client: &dyn remote::Remote,
    source: &Path,
    sub: &str,
    remove_source: bool,
    report: &mut dyn FnMut(u64) -> bool,
) -> Result<(), TransferError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        TransferError::Failed(crate::i18n::Message::args(
            "读不了源文件：{error}",
            &[("error", error.to_string())],
        ))
    })?;
    if metadata.is_dir() {
        client.mkdir(sub).map_err(TransferError::Failed)?;
        let children = fs::read_dir(source).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "列不了本地目录：{error}",
                &[("error", error.to_string())],
            ))
        })?;
        for child in children {
            let child =
                child.map_err(|error| TransferError::Failed(crate::i18n::Message::from(error)))?;
            let name = child.file_name().to_string_lossy().into_owned();
            let child_sub = if sub.is_empty() {
                name
            } else {
                format!("{sub}/{name}")
            };
            upload_entry(client, &child.path(), &child_sub, remove_source, report)?;
        }
        // 孩子已经各传各的、也各删各的了，到这里目录是空的，可以收尾。
        if remove_source {
            remove_entry(source).map_err(|error| {
                TransferError::Failed(crate::i18n::Message::args(
                    "已上传但删除本地源失败：{error}",
                    &[("error", error.to_string())],
                ))
            })?;
        }
        return Ok(());
    }

    let length = metadata.len();
    let modified = modified_of(&metadata);
    let journal = resume::journal();
    let key = resume::key(&show(source), &remote_source(client, sub));
    // 上传一样要占住：这一趟也会写账本、也会 sweep()，
    // 顺手把并发那趟正在写的暂存清掉就白干了。
    let _hold = journal.hold(&key);
    let can_resume = client.resumes_upload();

    // 上传的断点在**服务器上**，所以得多问一句：服务器那份现在有多长？
    // 问不出来、或者长度对不上（上一回是别的原因断的、中间有人动过），
    // 这本账就不能用——接着一个说不清长度的文件往后写太危险。
    let mut done = 0u64;
    if can_resume {
        if let Some(record) = journal.load(&key)
            && record.fresh(length, modified)
            && let Ok(Some(there)) = client.stat(sub)
            && there.size == record.done
            && record.done < length
        {
            done = record.done;
        }
        if done == 0 {
            journal.clear(&key);
        }
    }

    let file = fs::File::open(source).map_err(|error| {
        TransferError::Failed(crate::i18n::Message::args(
            "读不了源文件：{error}",
            &[("error", error.to_string())],
        ))
    })?;
    let mut file = file;
    if done > 0 {
        file.seek(io::SeekFrom::Start(done)).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "读不了源文件：{error}",
                &[("error", error.to_string())],
            ))
        })?;
        // 已经传上去的那部分先报一次，进度条从半路开始。
        if !report(done) {
            return Err(TransferError::Aborted);
        }
    }
    // 数一数这一趟实际交出去多少字节：断点要按这个记，
    // 不能按源文件的总长度（那会把没传的也算成传了）。
    let mut counting = Counting {
        inner: file,
        sent: 0,
    };
    let (result, was_aborted) = {
        let mut reader = Reporting::new(&mut counting, report);
        let result = if done > 0 {
            client.upload_from(sub, &mut reader, length, done)
        } else {
            client.upload(sub, &mut reader, length)
        };
        // 同下载那边：先问 `Reporting` 要，再让它掉出作用域。理由见 `Reporting`。
        (result, reader.aborted())
    };
    let sent = done + counting.sent;
    if let Err(error) = result {
        if was_aborted {
            // 同下载那边：`Reporting` 读完一块才报进度，那块若刚好盖到文件
            // 末尾，回调说停就停，其实服务器上已经是一份完整的文件。
            // 这时候报中止等于让用户白传一遍——下次接着传会从"长度正好
            // 等于总长"处起步，那本账本就永远用不上。
            if sent >= length {
                journal.clear(&key);
                if remove_source {
                    remove_entry(source).map_err(|error| {
                        TransferError::Failed(crate::i18n::Message::args(
                            "已上传但删除本地源失败：{error}",
                            &[("error", error.to_string())],
                        ))
                    })?;
                }
                return Ok(());
            }
            // 用户叫停：服务器上那一段是资产，留着，下次接着传。
            journal.save(
                &key,
                &resume::Record {
                    source: show(source),
                    size: length,
                    modified,
                    done: sent,
                },
            );
            journal.sweep();
            return Err(TransferError::Aborted);
        }
        // 真失败：把服务器上的残骸清掉。续传账本是给"我主动停一下"用的，
        // 一趟莫名失败的传输，它的断点说不清可不可信，从头传更稳妥。
        client.remove(sub).ok();
        journal.clear(&key);
        return Err(TransferError::Failed(error));
    }
    journal.clear(&key);
    if remove_source {
        remove_entry(source).map_err(|error| {
            TransferError::Failed(crate::i18n::Message::args(
                "已上传但删除本地源失败：{error}",
                &[("error", error.to_string())],
            ))
        })?;
    }
    Ok(())
}

/// 数一遭读过去多少字节：上传的断点只能这么记——
/// 服务器不会告诉客户端"你刚才传到我这儿了多少"。
struct Counting<R> {
    inner: R,
    sent: u64,
}

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.sent += read as u64;
        Ok(read)
    }
}

/// 边读边报字节数（上传用）/ 边写边报（下载用）的那一层包装。
///
/// 返回 false 就是用户按了中止：这里既把它变成一个 [`aborted`] 的 IO 错误
/// 让那一趟停下来，也**自己记进 [`Self::aborted`]**。
///
/// **为什么两个都要**：错误要穿过 `ureq` / `suppaftp` 才回到我们手里，
/// 而它们会把 IO 错误包一层再字符串化——`ureq::Error::Transport(..)`
/// 那边只留下 `transport.to_string()`，「已中止」到 `explain` 前面
/// 已经变成一句普通文本了。那时候再判「这一趟是不是被叫停的」，
/// 只能回去比文案，那正是 `Message::is_aborted` 要顶掉的毛病。
///
/// 自己记一份就绕开了整条猜谜链：这个类型是我们自己的，
/// 「我中止过」这个事实没有理由让它掉在半路上。
///
/// 错误种类仍必须是"不会触发重试"的那种——理由见 [`aborted`]。
struct Reporting<'a, R> {
    inner: R,
    report: &'a mut dyn FnMut(u64) -> bool,
    /// 用户按过中止。抄出来给调用方判，这一趟到底是"被叫停"还是"真失败"。
    aborted: bool,
}

impl<'a, R> Reporting<'a, R> {
    fn new(inner: R, report: &'a mut dyn FnMut(u64) -> bool) -> Self {
        Self {
            inner,
            report,
            aborted: false,
        }
    }

    /// 用户是不是按过中止。
    fn aborted(&self) -> bool {
        self.aborted
    }

    /// 记下中止，并给出那一趟要用的错误。
    fn stop(&mut self) -> io::Error {
        self.aborted = true;
        aborted()
    }
}

impl<R: std::io::Read> std::io::Read for Reporting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 && !(self.report)(read as u64) {
            return Err(self.stop());
        }
        Ok(read)
    }
}

impl<R: Write> Write for Reporting<'_, R> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        if written > 0 && !(self.report)(written as u64) {
            return Err(self.stop());
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// 拷贝/移动前先数一遍总条目数，让进度条从一开始就有正确的分母。
///
/// `client` 是源那一端在远程时的客户端：远程得靠列目录数，
/// 而列目录是要走一趟网络的。
pub fn count_entries(paths: &[PathBuf], client: Option<&dyn remote::Remote>) -> usize {
    paths.iter().map(|path| count_one(path, client)).sum()
}

/// 拷/移之前先数一遍总字节数：进度条、"还剩多久"都要这个分母。
///
/// 数不出来的（没权限、失效链接）按 0 算，界面退化成只看条目数。
pub fn count_bytes(paths: &[PathBuf], client: Option<&dyn remote::Remote>) -> u64 {
    paths.iter().map(|path| bytes_of(path, client)).sum()
}

fn bytes_of(path: &Path, client: Option<&dyn remote::Remote>) -> u64 {
    if let (Some(client), Some((_, sub))) = (client, remote::parse_location(path)) {
        return bytes_remote(client, &sub);
    }
    bytes_local(path)
}

/// 远程一棵子树的总字节数：一趟递归的 PROPFIND。
fn bytes_remote(client: &dyn remote::Remote, sub: &str) -> u64 {
    let Some(info) = client.stat(sub).ok().flatten() else {
        return 0;
    };
    if !info.is_dir {
        return info.size;
    }
    let Ok(children) = client.list(sub) else {
        return 0;
    };
    children
        .iter()
        .map(|child| bytes_remote(client, &child.path))
        .sum()
}

fn bytes_local(path: &Path) -> u64 {
    let Ok(link) = fs::symlink_metadata(path) else {
        return 0;
    };
    // 目录只看链接本身：跟着链接走进一个指向父目录的链接会无限递归。
    if link.is_dir() {
        let Ok(read_dir) = fs::read_dir(path) else {
            return 0;
        };
        return read_dir
            .flatten()
            .map(|entry| bytes_of(&entry.path(), None))
            .sum();
    }
    // 文件跟着链接走：真要拷的是链接指向的内容，
    // 链接自己那几十个字节（目标路径的长度）不是我们要搬的量。
    // 失效链接在这里取不到元数据，按 0 算。
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn count_one(path: &Path, client: Option<&dyn remote::Remote>) -> usize {
    if let (Some(client), Some((_, sub))) = (client, remote::parse_location(path)) {
        return count_remote(client, &sub);
    }
    count_local(path)
}

fn count_remote(client: &dyn remote::Remote, sub: &str) -> usize {
    let Some(info) = client.stat(sub).ok().flatten() else {
        return 1;
    };
    if !info.is_dir {
        return 1;
    }
    let Ok(children) = client.list(sub) else {
        return 1;
    };
    1 + children
        .iter()
        .map(|child| count_remote(client, &child.path))
        .sum::<usize>()
}

fn count_local(path: &Path) -> usize {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 1;
    };
    if !metadata.is_dir() {
        return 1;
    }

    let Ok(read_dir) = fs::read_dir(path) else {
        return 1;
    };
    1 + read_dir
        .flatten()
        .map(|entry| count_local(&entry.path()))
        .sum::<usize>()
}

// ---------------------------------------------------------------------------
// 撤销
// ---------------------------------------------------------------------------

/// 一条可撤销的操作记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryEntry {
    /// 新建的文件夹/文件：撤销就是删掉（文件夹必须还是空的）。
    Created { path: PathBuf },
    /// 重命名：撤销就是改回去。
    Renamed { from: PathBuf, to: PathBuf },
    /// 复制进来的副本：撤销就是删掉副本。
    Copied { created: Vec<PathBuf> },
    /// 移动：撤销就是移回原处。
    Moved { moves: Vec<(PathBuf, PathBuf)> },
    /// 批量重命名：撤销就是按记录全部改回去。
    BulkRenamed { moves: Vec<(PathBuf, PathBuf)> },
    /// 移进回收站：撤销就是从回收站放回原处。
    Trashed {
        originals: Vec<(PathBuf, SystemTime)>,
    },
}

impl HistoryEntry {
    /// 这一条记录涉及的所有路径（旧与新），撤销成功后界面据此刷新。
    pub fn paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Created { path } => vec![path.clone()],
            Self::Renamed { from, to } => vec![from.clone(), to.clone()],
            Self::Copied { created } => created.clone(),
            Self::Moved { moves } => moves
                .iter()
                .flat_map(|(from, to)| [from.clone(), to.clone()])
                .collect(),
            Self::BulkRenamed { moves } => moves
                .iter()
                .flat_map(|(from, to)| [from.clone(), to.clone()])
                .collect(),
            Self::Trashed { originals } => originals.iter().map(|(path, _)| path.clone()).collect(),
        }
    }

    /// 撤销按钮上的文案。
    ///
    /// **返回 `Msg` 不返回 `String`**：调用点把它嵌进「已撤销{what}」
    /// 那一句里（`Message::caused`）。返回 `String` 的话，那一层就得
    /// `to_string()`，内层的 key 当场没了——撤销按钮的文案在英文界面里
    /// 会显示成 `Undo undone: 重命名“a”为“b”`。
    pub fn describe(&self) -> crate::i18n::Msg {
        use crate::i18n::Message;
        match self {
            Self::Created { path } => {
                Message::args("新建“{name}”", &[("name", display_name(path))])
            }
            Self::Renamed { from, to } => Message::args(
                "重命名“{from}”为“{to}”",
                &[("from", display_name(from)), ("to", display_name(to))],
            ),
            Self::Copied { created } => {
                Message::args("复制 {count} 项", &[("count", created.len().to_string())])
            }
            Self::Moved { moves } => {
                Message::args("移动 {count} 项", &[("count", moves.len().to_string())])
            }
            Self::BulkRenamed { moves } => Message::args(
                "批量重命名 {count} 项",
                &[("count", moves.len().to_string())],
            ),
            Self::Trashed { originals } => Message::args(
                "移到回收站 {count} 项",
                &[("count", originals.len().to_string())],
            ),
        }
    }

    /// 执行撤销。失败时返回原因，且这一条记录仍然留在栈里（情况没变，重试也许还有意义）。
    ///
    /// `clients` 是这一条记录涉及的远程连接（按连接 id 索引）；
    /// 只涉及本地路径时它是空的，行为跟以前一样。
    pub fn undo(&self, clients: &HashMap<String, Client>) -> Result<(), crate::i18n::Msg> {
        match self {
            Self::Created { path } => remove_any(path, clients),
            Self::Renamed { from, to } => {
                if exists_any(to, clients) != Some(true) {
                    return Err(crate::i18n::Message::msg("原文件已经不在改名后的位置了"));
                }
                move_back(from, to, clients)
            }
            Self::Copied { created } => {
                for path in created {
                    if exists_any(path, clients) == Some(true) {
                        remove_any(path, clients)?;
                    }
                }
                Ok(())
            }
            Self::Moved { moves } => {
                // 逆序搬回去，避免序号类的落点互相踩。
                for (from, to) in moves.iter().rev() {
                    move_back(from, to, clients)?;
                }
                Ok(())
            }
            Self::BulkRenamed { moves } => {
                // 逆序改回去，避免序号类改名互相踩。
                for (from, to) in moves.iter().rev() {
                    if !to.exists() {
                        return Err(crate::i18n::Message::args(
                            "“{name}”已经不在改名后的位置了",
                            &[("name", display_name(to))],
                        ));
                    }
                    fs::rename(to, from).map_err(crate::i18n::Message::from)?;
                }
                Ok(())
            }
            Self::Trashed { originals } => {
                for (original, trashed_at) in originals {
                    if original.exists() {
                        continue; // 原地又出现了一个同名项，不要覆盖它
                    }
                    restore_from_trash(original, *trashed_at)?;
                }
                Ok(())
            }
        }
    }
}

/// 本地或远程，这一项在不在。`None` = 问不出来（连接没了）。
fn exists_any(path: &Path, clients: &HashMap<String, Client>) -> Option<bool> {
    let Some((id, sub)) = remote::parse_location(path) else {
        return Some(path.exists());
    };
    let client = clients.get(&id)?;
    Some(client.stat(&sub).unwrap_or(None).is_some())
}

/// 本地或远程，删掉这一项。
fn remove_any(path: &Path, clients: &HashMap<String, Client>) -> Result<(), crate::i18n::Msg> {
    let Some((id, sub)) = remote::parse_location(path) else {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| crate::i18n::Message::msg("这一项已经不存在了"))?;
        if metadata.is_dir() {
            let empty = fs::read_dir(path)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if !empty {
                return Err(crate::i18n::Message::msg(
                    "文件夹里已经有了新内容，先清空再撤销",
                ));
            }
        }
        return remove_entry(path).map_err(crate::i18n::Message::from);
    };
    let client = clients
        .get(&id)
        .ok_or_else(|| crate::i18n::Message::msg("这条远程连接已经不在了"))?;
    client.remove(&sub)
}

/// 撤销一次移动：把 `to` 搬回 `from`。两端各可能是本地或远程。
fn move_back(
    from: &Path,
    to: &Path,
    clients: &HashMap<String, Client>,
) -> Result<(), crate::i18n::Msg> {
    let from_remote = remote::parse_location(from);
    let to_remote = remote::parse_location(to);
    // 先按“两边各是什么位置”取好客户端，后面哪个分支用得上就用。
    let sides = RemoteSides {
        source: to_remote
            .as_ref()
            .and_then(|(id, _)| clients.get(id).cloned()),
        dest: from_remote
            .as_ref()
            .and_then(|(id, _)| clients.get(id).cloned()),
    };
    match (from_remote, to_remote) {
        (None, None) => {
            if let Some(parent) = from.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::rename(to, from).map_err(crate::i18n::Message::from)
        }
        // 同一台服务器：让服务器自己搬回去，一趟 MOVE 就完事。
        (Some((from_id, from_sub)), Some((to_id, to_sub))) if from_id == to_id => {
            let client = clients
                .get(&from_id)
                .ok_or_else(|| crate::i18n::Message::msg("这条远程连接已经不在了"))?;
            client.rename(&to_sub, &from_sub)
        }
        // 一边本地一边远程（或跨服务器）：走一趟真的下载/上传。
        // 搬回去之后 `to` 那一端不该还留着一份，所以是"移动"不是"复制"。
        // 撤销跑在后台线程上，不用报进度，给个一律放行的上报器。
        _ => {
            let mut report = |_bytes: u64| true;
            match transfer_one(to, from, true, &sides, &mut report) {
                Ok(()) => Ok(()),
                Err(TransferError::Aborted) => Err(crate::i18n::Message::aborted()),
                Err(TransferError::Failed(message)) => Err(message),
            }
        }
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 撤销栈。只记能撤的；失败的操作不进栈。
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    pub fn push(&mut self, entry: HistoryEntry) {
        self.entries.push(entry);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 栈顶操作的描述，用于“撤销 XXX”按钮。
    pub fn last_describe(&self) -> Option<crate::i18n::Msg> {
        self.entries.last().map(HistoryEntry::describe)
    }

    /// 栈顶操作涉及的路径（撤销前后都要刷新）。
    pub fn last_paths(&self) -> Vec<PathBuf> {
        self.entries
            .last()
            .map(HistoryEntry::paths)
            .unwrap_or_default()
    }

    /// 弹出并执行撤销，返回被撤销操作的描述。
    pub fn pop_undo(
        &mut self,
        clients: &HashMap<String, Client>,
    ) -> Result<crate::i18n::Msg, crate::i18n::Msg> {
        let entry = self
            .entries
            .pop()
            .ok_or_else(|| crate::i18n::Message::msg("没有可撤销的操作"))?;
        match entry.undo(clients) {
            Ok(()) => Ok(entry.describe()),
            Err(error) => {
                // 撤不动就放回栈里，下一次重试。
                self.entries.push(entry);
                Err(error)
            }
        }
    }

    /// 路径被改名后，同步修正栈里指向旧路径的记录，
    /// 否则“撤销新建”会去找一个早就改名掉的文件夹。
    pub fn note_renamed(&mut self, from: &Path, to: &Path) {
        for entry in &mut self.entries {
            match entry {
                HistoryEntry::Created { path } => {
                    if *path == from {
                        *path = to.to_path_buf();
                    }
                }
                HistoryEntry::Renamed { from: f, to: t } => {
                    if *f == from {
                        *f = to.to_path_buf();
                    }
                    if *t == from {
                        *t = to.to_path_buf();
                    }
                }
                HistoryEntry::Copied { created } => {
                    for path in created.iter_mut() {
                        if *path == from {
                            *path = to.to_path_buf();
                        }
                    }
                }
                HistoryEntry::Moved { moves } => {
                    for (f, t) in moves.iter_mut() {
                        if *f == from {
                            *f = to.to_path_buf();
                        }
                        if *t == from {
                            *t = to.to_path_buf();
                        }
                    }
                }
                HistoryEntry::BulkRenamed { moves } => {
                    for (f, t) in moves.iter_mut() {
                        if *f == from {
                            *f = to.to_path_buf();
                        }
                        if *t == from {
                            *t = to.to_path_buf();
                        }
                    }
                }
                HistoryEntry::Trashed { .. } => {}
            }
        }
    }

    /// 路径被删掉后，把对应的记录降级：新建的记录直接失效，
    /// 移动/复制记录把消失的项摘掉，空了就移除整条。
    pub fn note_removed(&mut self, path: &Path) {
        self.entries.retain_mut(|entry| match entry {
            HistoryEntry::Created { path: created } => *created != path,
            HistoryEntry::Renamed { from, to } => *from != path && *to != path,
            HistoryEntry::Copied { created } => {
                created.retain(|item| item != path);
                !created.is_empty()
            }
            HistoryEntry::Moved { moves } => {
                moves.retain(|(f, t)| f != path && t != path);
                !moves.is_empty()
            }
            HistoryEntry::BulkRenamed { moves } => {
                moves.retain(|(f, t)| f != path && t != path);
                !moves.is_empty()
            }
            HistoryEntry::Trashed { originals } => {
                originals.retain(|(item, _)| item != path);
                !originals.is_empty()
            }
        });
    }
}

/// 名字是否合法（新建/重命名共用）。
pub fn valid_name(name: &str) -> Result<(), crate::i18n::Msg> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(crate::i18n::Message::msg("名称不能为空"));
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(crate::i18n::Message::msg("名称不能包含路径分隔符"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---- 命名与冲突 ----

    #[test]
    fn fresh_folder_name_skips_existing() {
        let root = temp();
        fs::create_dir(root.path().join("未命名文件夹")).unwrap();
        fs::create_dir(root.path().join("未命名文件夹 2")).unwrap();

        assert_eq!(
            fresh_folder_path(root.path()),
            root.path().join("未命名文件夹 3")
        );
    }

    #[test]
    fn unique_sibling_keeps_extension_and_hidden_dot_files() {
        let root = temp();
        let a = root.path().join("a.txt");
        let hidden = root.path().join(".gitignore");
        fs::write(&a, "x").unwrap();
        fs::write(&hidden, "x").unwrap();

        assert_eq!(unique_sibling(&a), root.path().join("a 2.txt"));
        assert_eq!(unique_sibling(&hidden), root.path().join(".gitignore 2"));
    }

    #[test]
    fn conflict_policies_pick_different_targets() {
        let root = temp();
        let target = root.path().join("a.txt");
        fs::write(&target, "x").unwrap();

        assert_eq!(
            resolve_conflict(&target, ConflictPolicy::KeepBoth),
            Some(root.path().join("a 2.txt"))
        );
        assert_eq!(
            resolve_conflict(&target, ConflictPolicy::Replace),
            Some(target.clone())
        );
        assert_eq!(resolve_conflict(&target, ConflictPolicy::Skip), None);
    }

    #[test]
    fn rename_rejects_empty_and_separators() {
        let root = temp();
        let file = root.path().join("a.txt");
        fs::write(&file, "x").unwrap();

        assert!(rename(&file, "  ").is_err());
        assert!(rename(&file, "a/b").is_err());
        assert!(rename(&file, "..").is_err());
        assert!(rename(&file, "b.txt").is_ok());
        assert!(root.path().join("b.txt").exists());
    }

    #[test]
    fn rename_rejects_existing_target() {
        let root = temp();
        fs::write(root.path().join("a.txt"), "x").unwrap();
        fs::write(root.path().join("b.txt"), "x").unwrap();

        assert!(rename(&root.path().join("a.txt"), "b.txt").is_err());
    }

    #[test]
    fn rename_allows_case_only_change() {
        let root = temp();
        let file = root.path().join("a.txt");
        fs::write(&file, "x").unwrap();

        // 大小写改名在大小写不敏感的文件系统上目标是“自己”，必须放行。
        let renamed = rename(&file, "A.txt").expect("case-only rename");
        assert_eq!(renamed, root.path().join("A.txt"));
    }

    // ---- 拷贝 / 移动 ----

    #[test]
    fn copy_entries_copies_files_and_directories() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();
        fs::create_dir(src.join("nested")).unwrap();
        fs::write(src.join("nested/b.txt"), "bb").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let mut labels = Vec::new();
        let outcome = copy_entries(
            &[src.join("a.txt"), src.join("nested")],
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| {
                labels.push(progress.label.clone());
                true
            },
        );

        assert_eq!(outcome.failed, vec![]);
        assert_eq!(outcome.done, 2);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(dest.join("nested/b.txt")).unwrap(), "bb");
        assert!(src.join("a.txt").exists(), "拷贝不删源");
        assert_eq!(labels.first().map(String::as_str), Some("正在拷贝 a.txt"));
    }

    #[test]
    fn copy_entries_keep_both_never_overwrites() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "new").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("a.txt"), "old").unwrap();

        let outcome = copy_entries(
            &[src.join("a.txt")],
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 1);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("a 2.txt")).unwrap(), "new");
        assert_eq!(outcome.touched, vec![dest.join("a 2.txt")]);
    }

    #[test]
    fn copy_entries_replace_overwrites() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "new").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("a.txt"), "old").unwrap();

        let outcome = copy_entries(
            &[src.join("a.txt")],
            &dest,
            ConflictPolicy::Replace,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 1);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "new");
        assert!(!dest.join("a 2.txt").exists());
    }

    #[test]
    fn copy_entries_skip_leaves_target_alone() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "new").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("a.txt"), "old").unwrap();

        let outcome = copy_entries(
            &[src.join("a.txt")],
            &dest,
            ConflictPolicy::Skip,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "old");
    }

    #[test]
    fn move_entries_removes_the_source() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let outcome = move_entries(
            &[src.join("a.txt")],
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 1);
        assert!(!src.join("a.txt").exists(), "移动要删源");
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "aaa");
        assert_eq!(outcome.touched, vec![dest.join("a.txt")]);
    }

    #[test]
    fn moving_a_folder_into_itself_is_refused() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir_all(src.join("inner")).unwrap();

        let outcome = move_entries(
            std::slice::from_ref(&src),
            &src.join("inner"),
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed.len(), 1);
        assert!(src.join("inner").is_dir());
    }

    #[test]
    fn progress_callback_can_cancel_midway() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        for ix in 0..6 {
            fs::write(src.join(format!("f{ix}.txt")), "x").unwrap();
        }

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        // 按"已经完成几项"来叫停，而不是数回调被调了几次：
        // 回调现在每一项会来好几次（每拷完一个数据块一次），
        // 数次数会把测试钉死在实现细节上。
        let outcome = copy_entries(
            &(0..6)
                .map(|ix| src.join(format!("f{ix}.txt")))
                .collect::<Vec<_>>(),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| progress.done < 2,
        );

        assert_eq!(outcome.done, 2, "第 3 项开始前被叫停");
        assert_eq!(outcome.skipped, 4, "剩下的全部记为跳过");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
    }

    // ---- 跨本地与远程（真连一次假服务器）----

    /// 起一个假服务器并连上它，返回客户端和"它都收到了什么"。
    fn remote_client() -> (Client, remote::fake::Server) {
        let server = remote::fake::start();
        let client = remote::open(&remote::fake::config(&server.url), "secret").unwrap();
        (client, server)
    }

    fn seen_requests(server: &remote::fake::Server) -> Vec<String> {
        server.seen.lock().unwrap().clone()
    }

    #[test]
    fn copying_local_files_up_to_a_remote_folder() {
        let (client, server) = remote_client();
        let root = temp();
        let local = root.path().join("a.txt");
        fs::write(&local, "hello").unwrap();

        // 目标写成 `dav://t1/folder`：窗格里就是这么一个路径，
        // 拼上文件名后上传的地址应该是 folder/a.txt。
        let dest = remote::location("t1", "folder");
        let sides = RemoteSides {
            source: None,
            dest: Some(client),
        };
        let outcome = copy_entries(
            std::slice::from_ref(&local),
            &dest,
            ConflictPolicy::Replace,
            &sides,
            &mut |_| true,
        );

        assert_eq!(outcome.failed, vec![]);
        assert_eq!(outcome.done, 1);
        assert_eq!(
            seen_requests(&server),
            vec![
                // 判重：服务器说没有，才轮到真的上传。
                "PROPFIND /dav/folder/a.txt".to_string(),
                "PUT /dav/folder/a.txt".to_string(),
            ]
        );
        assert!(local.exists(), "上传不删本地那份");
    }

    #[test]
    fn moving_local_files_up_to_a_remote_folder_deletes_the_local_copy() {
        let (client, server) = remote_client();
        let root = temp();
        let local = root.path().join("a.txt");
        fs::write(&local, "hello").unwrap();

        let dest = remote::location("t1", "folder");
        let sides = RemoteSides {
            source: None,
            dest: Some(client),
        };
        let outcome = move_entries(
            std::slice::from_ref(&local),
            &dest,
            ConflictPolicy::Replace,
            &sides,
            &mut |_| true,
        );

        assert_eq!(outcome.failed, vec![]);
        assert_eq!(outcome.done, 1);
        // 剪切就是剪切：传上去以后本地这份要没了。
        assert!(!local.exists());
        assert!(seen_requests(&server).ends_with(&["PUT /dav/folder/a.txt".to_string()]));
    }

    #[test]
    fn moving_a_remote_file_down_deletes_it_up_there() {
        let (client, server) = remote_client();
        let root = temp();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let source = remote::location("t1", "folder/notes.txt");
        let sides = RemoteSides {
            source: Some(client),
            dest: None,
        };
        let outcome = move_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::Replace,
            &sides,
            &mut |_| true,
        );

        assert_eq!(outcome.failed, vec![]);
        assert_eq!(outcome.done, 1);
        assert_eq!(
            fs::read_to_string(dest.join("notes.txt")).unwrap(),
            "hello dav"
        );
        // 移动 = 下载 + 删掉服务器上那份，顺序不能反。
        assert_eq!(
            seen_requests(&server),
            vec![
                "PROPFIND /dav/folder/notes.txt".to_string(),
                "PROPFIND /dav/folder/notes.txt".to_string(),
                "GET /dav/folder/notes.txt".to_string(),
                "DELETE /dav/folder/notes.txt".to_string(),
            ]
        );
    }

    #[test]
    fn moving_within_one_server_is_a_single_request() {
        let (client, server) = remote_client();
        let source = remote::location("t1", "folder/a.txt");
        let dest = remote::location("t1", "folder/sub");
        let sides = RemoteSides {
            source: Some(client.clone()),
            dest: Some(client),
        };
        let outcome = move_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::Replace,
            &sides,
            &mut |_| true,
        );

        assert_eq!(outcome.failed, vec![]);
        assert_eq!(outcome.done, 1);
        // 同一台服务器之间没有"下载再上传"这回事：一趟 MOVE 就完事。
        // 先数总字节（问了源），再判目标重不重，最后才动手。
        assert_eq!(
            seen_requests(&server),
            vec![
                "PROPFIND /dav/folder/a.txt".to_string(),
                "PROPFIND /dav/folder/sub/a.txt".to_string(),
                "MOVE /dav/folder/a.txt".to_string(),
            ]
        );
    }

    #[test]
    fn carrying_between_two_servers_is_refused() {
        let first = remote::fake::start();
        let second = remote::fake::start();
        let from = remote::open(&remote::fake::config_with("t1", &first.url), "secret").unwrap();
        let to = remote::open(&remote::fake::config_with("t2", &second.url), "secret").unwrap();

        let source = remote::location("t1", "folder/a.txt");
        let dest = remote::location("t2", "folder");
        let sides = RemoteSides {
            source: Some(from),
            dest: Some(to),
        };
        let outcome = move_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::Replace,
            &sides,
            &mut |_| true,
        );

        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed.len(), 1);
        assert!(
            outcome.failed[0].1.to_string().contains("两个不同的服务器"),
            "要说清为什么不行：{}",
            outcome.failed[0].1
        );
        // 拒绝就是拒绝：第二台服务器上连一个写请求都不该有
        // （判重那次 PROPFIND 是只读的，不算动手）。
        let writes: Vec<_> = seen_requests(&second)
            .into_iter()
            .filter(|it| !it.starts_with("PROPFIND"))
            .collect();
        assert!(writes.is_empty(), "不该在目标服务器上留下东西：{writes:?}");
    }

    #[test]
    fn undo_moves_a_remote_file_back_to_local() {
        let (client, server) = remote_client();
        let root = temp();
        let local = root.path().join("notes.txt");
        let remote_at = remote::location("t1", "folder/notes.txt");

        let mut history = History::default();
        history.push(HistoryEntry::Moved {
            moves: vec![(local.clone(), remote_at.clone())],
        });
        let mut clients = HashMap::new();
        clients.insert("t1".to_string(), client);
        history.pop_undo(&clients).unwrap();

        assert_eq!(fs::read_to_string(&local).unwrap(), "hello dav");
        // 撤销移动 = 下载回来 + 删掉远程那份。
        assert_eq!(
            seen_requests(&server),
            vec![
                "PROPFIND /dav/folder/notes.txt".to_string(),
                "GET /dav/folder/notes.txt".to_string(),
                "DELETE /dav/folder/notes.txt".to_string(),
            ]
        );
    }

    #[test]
    fn undo_moves_a_local_file_back_up_to_the_server() {
        let (client, server) = remote_client();
        let root = temp();
        // 模拟一次"远程 -> 本地"的移动已经发生：文件此刻在本地。
        let local = root.path().join("notes.txt");
        fs::write(&local, "hello dav").unwrap();
        let remote_at = remote::location("t1", "folder/notes.txt");

        let mut history = History::default();
        history.push(HistoryEntry::Moved {
            moves: vec![(remote_at.clone(), local.clone())],
        });
        let mut clients = HashMap::new();
        clients.insert("t1".to_string(), client);
        history.pop_undo(&clients).unwrap();

        // 撤销 = 传回服务器 + 删掉本地那份。
        assert!(!local.exists(), "本地那份不该留着");
        assert_eq!(
            seen_requests(&server),
            vec!["PUT /dav/folder/notes.txt".to_string()]
        );
    }

    // ---- 后台任务的控制手柄 ----

    #[test]
    fn control_runs_pauses_and_aborts() {
        let control = Control::new();
        assert_eq!(control.state(), ControlState::Running);
        assert!(control.should_continue());

        control.pause();
        assert_eq!(control.state(), ControlState::Paused);

        control.resume();
        assert_eq!(control.state(), ControlState::Running);

        control.abort();
        assert!(!control.should_continue());

        // 中止是单向的：发出去之后"继续"不该把它救回来，
        // 否则用户点了停止，界面上还会显示正在跑。
        control.resume();
        assert_eq!(control.state(), ControlState::Aborting);
    }

    #[test]
    fn control_handles_share_one_state() {
        let ui = Control::new();
        let worker = ui.clone();

        worker.pause();
        assert_eq!(
            ui.state(),
            ControlState::Paused,
            "工作线程那一份改了，UI 这边要看得见"
        );

        ui.abort();
        assert!(!worker.should_continue());
    }

    #[test]
    fn a_paused_worker_waits_until_it_is_resumed() {
        let control = Control::new();
        let worker = control.clone();
        control.pause();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let continued = worker.should_continue();
            tx.send(continued).expect("主线程还在等");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "暂停期间 should_continue 该一直堵着"
        );
        control.resume();
        let continued = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("恢复之后工作线程该继续");
        assert!(continued);
    }

    // ---- 字节数 ----

    #[test]
    fn count_bytes_sums_every_file_in_a_tree() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), "12345").unwrap();
        fs::write(src.join("nested/b.txt"), "1234567890").unwrap();

        assert_eq!(count_bytes(std::slice::from_ref(&src), None), 15);
        assert_eq!(count_bytes(&[src.join("a.txt"), src], None), 20);
    }

    #[cfg(unix)]
    #[test]
    fn count_bytes_follows_a_link_and_forgets_a_broken_one() {
        use std::os::unix::fs::symlink;

        let root = temp();
        fs::write(root.path().join("real.txt"), "12345").unwrap();
        symlink(root.path().join("real.txt"), root.path().join("link.txt")).unwrap();
        symlink(root.path().join("nowhere"), root.path().join("gone.txt")).unwrap();

        let paths = vec![root.path().join("link.txt"), root.path().join("gone.txt")];
        // 链接算它指向的内容（5 字节），失效链接算 0。
        assert_eq!(count_bytes(&paths, None), 5);
    }

    #[test]
    fn copying_reports_the_bytes_it_has_written() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "12345").unwrap();
        fs::write(src.join("b.txt"), "1234567890").unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let mut last = None;
        copy_entries(
            &[src.join("a.txt"), src.join("b.txt")],
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| {
                last = Some(progress.clone());
                true
            },
        );

        let last = last.expect("至少汇报过一次");
        assert_eq!(last.bytes_total, 15, "分母是总字节数");
        assert_eq!(last.bytes_done, 15);
        assert_eq!((last.done, last.total), (2, 2));
    }

    #[test]
    fn aborting_halfway_through_a_file_leaves_no_partial_copy() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        // 1 MiB 一个数据块，写到第二块就叫停。
        fs::write(src.join("big.bin"), vec![0u8; 2 * 1024 * 1024 + 1024]).unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let outcome = copy_entries(
            std::slice::from_ref(&src.join("big.bin")),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| progress.bytes_done < 2 * 1024 * 1024,
        );

        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.skipped, 1);
        assert!(outcome.failed.is_empty(), "中止不算失败，不该进重试列表");
        assert!(
            !dest.join("big.bin").exists(),
            "中止之后不留半个文件：它被删掉了"
        );
    }

    #[test]
    fn aborting_inside_a_folder_keeps_what_was_already_copied() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        for ix in 0..3 {
            fs::write(src.join(format!("f{ix}.bin")), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        // 拷到第二个文件的第一块就叫停。
        let outcome = copy_entries(
            std::slice::from_ref(&src),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| progress.bytes_done < 4 * 1024 * 1024,
        );

        assert_eq!(outcome.done, 0);
        let copied = dest.join("src");
        assert_eq!(
            fs::metadata(copied.join("f0.bin"))
                .expect("第一个该完整留着")
                .len(),
            3 * 1024 * 1024,
        );
        assert!(
            !copied.join("f1.bin").exists(),
            "拷了一半的那个要删掉，不留截断的文件"
        );
        assert!(!copied.join("f2.bin").exists());
    }

    #[test]
    fn count_entries_counts_recursively() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir_all(src.join("nested/deep")).unwrap();
        fs::write(src.join("a.txt"), "x").unwrap();
        fs::write(src.join("nested/b.txt"), "x").unwrap();
        fs::write(src.join("nested/deep/c.txt"), "x").unwrap();

        assert_eq!(count_entries(std::slice::from_ref(&src), None), 6);
        assert_eq!(count_entries(&[src.join("a.txt"), src], None), 7);
    }

    // ---- 撤销 ----

    #[test]
    fn undo_create_removes_the_folder() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created {
            path: folder.clone(),
        });
        // 判 `Msg` 的显示文案（`Display` 恒给中文，与语言无关）。
        assert_eq!(
            history.last_describe().map(|it| it.to_string()),
            Some("新建“未命名文件夹”".to_string())
        );

        assert_eq!(
            history.pop_undo(&HashMap::new()).unwrap().to_string(),
            "新建“未命名文件夹”"
        );
        assert!(!folder.exists());
        assert!(history.is_empty());
    }

    #[test]
    fn undo_create_refuses_a_folder_that_gained_content() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();
        fs::write(folder.join("later.txt"), "x").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created {
            path: folder.clone(),
        });

        assert!(history.pop_undo(&HashMap::new()).is_err());
        assert!(history.last_describe().is_some(), "撤不动要留在栈里");
        assert!(folder.exists());
    }

    #[test]
    fn undo_rename_moves_it_back() {
        let root = temp();
        let file = root.path().join("a.txt");
        fs::write(&file, "x").unwrap();

        let renamed = rename(&file, "b.txt").unwrap();
        let mut history = History::default();
        history.push(HistoryEntry::Renamed {
            from: file.clone(),
            to: renamed,
        });

        history.pop_undo(&HashMap::new()).unwrap();
        assert!(file.exists());
        assert!(!root.path().join("b.txt").exists());
    }

    #[test]
    fn undo_copy_deletes_the_copies() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "x").unwrap();
        fs::create_dir(src.join("d")).unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let outcome = copy_entries(
            &[src.join("a.txt"), src.join("d")],
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |_| true,
        );

        let mut history = History::default();
        history.push(HistoryEntry::Copied {
            created: outcome.touched.clone(),
        });
        history.pop_undo(&HashMap::new()).unwrap();

        assert!(!dest.join("a.txt").exists());
        assert!(!dest.join("d").exists());
        assert!(src.join("a.txt").exists(), "原件不受影响");
    }

    #[test]
    fn undo_move_puts_everything_back() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();
        fs::write(src.join("b.txt"), "bbb").unwrap();

        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let sources = vec![src.join("a.txt"), src.join("b.txt")];
        let outcome = move_entries(
            &sources,
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |_| true,
        );
        assert_eq!(outcome.done, 2);

        // 撤销记录按“源 -> 目标”成对记住，放回时反着来。
        let moves = sources
            .iter()
            .map(|source| {
                let name = source.file_name().unwrap().to_string_lossy().into_owned();
                (source.clone(), dest.join(name))
            })
            .collect();
        let mut history = History::default();
        history.push(HistoryEntry::Moved { moves });

        history.pop_undo(&HashMap::new()).unwrap();
        assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(src.join("b.txt")).unwrap(), "bbb");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn undo_bulk_rename_restores_every_name() {
        let root = temp();
        let names = ["a.txt", "b.txt", "c.txt"];
        let paths: Vec<PathBuf> = names.iter().map(|name| root.path().join(name)).collect();
        for path in &paths {
            fs::write(path, "x").unwrap();
        }

        // 模拟一次批量重命名：整批压成一条记录。
        let moves: Vec<(PathBuf, PathBuf)> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                (
                    path.clone(),
                    root.path().join(format!("IMG_{}.txt", index + 1)),
                )
            })
            .collect();
        let mut history = History::default();
        history.push(HistoryEntry::BulkRenamed {
            moves: moves.clone(),
        });

        for (from, to) in &moves {
            fs::rename(from, to).unwrap();
        }

        history.pop_undo(&HashMap::new()).unwrap();
        for path in &paths {
            assert!(path.exists(), "{} 应该被改回原名", path.display());
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), paths.len());
    }

    #[test]
    fn history_self_heals_after_a_bulk_rename() {
        let root = temp();
        let original = root.path().join("a.txt");
        let renamed = root.path().join("b.txt");
        fs::write(&original, "x").unwrap();
        // 先按批量重命名真的落盘一次，后面的“又被改名”才有对象。
        fs::rename(&original, &renamed).unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::BulkRenamed {
            moves: vec![(original.clone(), renamed.clone())],
        });

        // 这批之后，其中一个产物又被改成了别的名字：栈里的旧路径要跟着更新。
        let later = root.path().join("c.txt");
        fs::rename(&renamed, &later).unwrap();
        history.note_renamed(&renamed, &later);
        assert_eq!(history.last_paths(), vec![original.clone(), later.clone()]);

        // 产物被删掉：涉及它的记录要失效。
        fs::remove_file(&later).unwrap();
        history.note_removed(&later);
        assert!(history.is_empty(), "记录涉及的路径消失后应被清理");
    }

    #[test]
    fn note_renamed_keeps_undo_pointing_at_the_new_name() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();
        let renamed = rename(&folder, "改名了").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created {
            path: folder.clone(),
        });
        history.note_renamed(&folder, &renamed);

        assert_eq!(
            history.entries[0],
            HistoryEntry::Created {
                path: renamed.clone()
            }
        );
        history.pop_undo(&HashMap::new()).unwrap();
        assert!(!renamed.exists());
    }

    #[test]
    fn note_removed_drops_stale_entries() {
        let root = temp();
        let gone = root.path().join("gone.txt");

        let mut history = History::default();
        history.push(HistoryEntry::Created { path: gone.clone() });
        history.push(HistoryEntry::Copied {
            created: vec![gone.clone(), root.path().join("kept.txt")],
        });

        history.note_removed(&gone);
        assert_eq!(
            history.last_describe().map(|it| it.to_string()),
            Some("复制 1 项".to_string()),
            "只删失效的那条"
        );
        assert_eq!(
            history.entries[0],
            HistoryEntry::Copied {
                created: vec![root.path().join("kept.txt")]
            }
        );
    }

    #[test]
    fn valid_name_rejects_separators() {
        assert!(valid_name("ok.txt").is_ok());
        assert!(valid_name(" ").is_err());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("a\\b").is_err());
    }

    // ---- 回收站恢复（macOS 启发式）----

    #[cfg(target_os = "macos")]
    #[test]
    fn trash_name_matches_accepts_originals_and_numbered_duplicates() {
        assert!(trash_name_matches("a.txt", "a", ".txt"));
        assert!(trash_name_matches("a 2.txt", "a", ".txt"));
        assert!(trash_name_matches("a 10.txt", "a", ".txt"));
        // 点号开头的名字整体是主干，不是扩展名。
        assert!(trash_name_matches(".gitignore", ".gitignore", ""));
        // 别的名字、别的序号格式都不算。
        assert!(!trash_name_matches("ab.txt", "a", ".txt"));
        assert!(!trash_name_matches("a2.txt", "a", ".txt"));
        assert!(!trash_name_matches("a x.txt", "a", ".txt"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restore_from_trash_dir_moves_the_item_back() {
        let root = temp();
        let original_parent = root.path().join("work");
        fs::create_dir(&original_parent).unwrap();
        let original = original_parent.join("报告.txt");

        // 模拟回收站：文件已经躺在里面。
        let trash_dir = root.path().join(".Trash");
        fs::create_dir(&trash_dir).unwrap();
        fs::write(trash_dir.join("报告.txt"), "content").unwrap();

        restore_from_trash_dir(&trash_dir, &original, SystemTime::now()).unwrap();

        assert_eq!(fs::read_to_string(&original).unwrap(), "content");
        assert!(!trash_dir.join("报告.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restore_ignores_items_trashed_before_this_deletion() {
        let root = temp();
        let original_parent = root.path().join("work");
        fs::create_dir(&original_parent).unwrap();
        let original = original_parent.join("old.txt");

        let trash_dir = root.path().join(".Trash");
        fs::create_dir(&trash_dir).unwrap();
        // 同名项是更早删的：ctime 早于“本次删除时刻”。
        fs::write(trash_dir.join("old.txt"), "stale").unwrap();

        // 声称一小时后才删——现有的同名项都太旧，不该被放回。
        let much_later = SystemTime::now() + Duration::from_secs(3600);
        let error = restore_from_trash_dir(&trash_dir, &original, much_later).unwrap_err();

        assert!(
            error.to_string().contains("回收站"),
            "应当给出可理解的失败原因：{error}"
        );
        assert!(trash_dir.join("old.txt").exists(), "旧项不能被误放回");
    }

    // ---- 断点续传 ----

    /// 比一个数据块多一点的 data：半路叫停才停得在数据块中间。
    ///
    /// 只多一百来字节是刻意的：数据块有 1 MB，测试跑在可能只剩
    /// 一两百兆空闲的机器上，没必要为验一件事先写出两个大文件。
    fn chunky() -> Vec<u8> {
        (0..COPY_CHUNK + 100).map(|ix| (ix % 251) as u8).collect()
    }

    #[test]
    fn an_aborted_copy_resumes_from_where_it_stopped() {
        let root = temp();
        let src = root.path().join("big.bin");
        let data = chunky();
        fs::write(&src, &data).unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let target = dest.join("big.bin");

        // 第一趟：拷完一个数据块就叫停。
        let outcome = copy_entries(
            std::slice::from_ref(&src),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| progress.bytes_done < COPY_CHUNK as u64,
        );
        assert_eq!(outcome.done, 0, "没拷完不算完成");
        assert_eq!(outcome.skipped, 1);
        assert!(
            !target.exists(),
            "半截文件不该出现在目标目录里——它该在续传暂存处"
        );

        // 第二趟：同一对 (源, 目标) 接着传。
        let mut started_at = None;
        let outcome = copy_entries(
            std::slice::from_ref(&src),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| {
                if progress.bytes_done > 0 && started_at.is_none() {
                    started_at = Some(progress.bytes_done);
                }
                true
            },
        );

        assert_eq!(outcome.done, 1);
        assert_eq!(fs::read(&target).unwrap(), data, "接上去的得是完整的一份");
        assert_eq!(
            started_at,
            Some(COPY_CHUNK as u64),
            "第二趟只该报剩下那一截：续传不是把整份重新读一遍"
        );
    }

    #[test]
    fn a_changed_source_invalidates_the_resume_point() {
        let root = temp();
        let src = root.path().join("big.bin");
        let data = chunky();
        fs::write(&src, &data).unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let aborted = copy_entries(
            std::slice::from_ref(&src),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| progress.bytes_done < COPY_CHUNK as u64,
        );
        assert_eq!(aborted.skipped, 1);

        // 源被改过了：续传点作废，第二趟得从 0 开始。
        let mut changed = data.clone();
        changed[0] = changed[0].wrapping_add(1);
        fs::write(&src, &changed).unwrap();

        let mut started_at = None;
        let outcome = copy_entries(
            std::slice::from_ref(&src),
            &dest,
            ConflictPolicy::KeepBoth,
            &RemoteSides::default(),
            &mut |progress| {
                if progress.bytes_done > 0 && started_at.is_none() {
                    started_at = Some(progress.bytes_done);
                }
                true
            },
        );

        assert_eq!(outcome.done, 1);
        assert_eq!(fs::read(dest.join("big.bin")).unwrap(), changed);
        assert_eq!(started_at, Some(COPY_CHUNK as u64), "这一趟是整份重拷");
    }

    /// `std::io::copy` 一次读多少的下界，用来判断"断点是不是已经下完了"。
    ///
    /// 实测这个假服务器上每块是 8091 / 8333 字节这样——**不是整数 8K**，
    /// 因为块边界由 socket 缓冲和调度决定。取 8 KB 是拿它的量级，
    /// 不是断言它就是 8192。
    fn io_copy_buffer() -> u64 {
        8 * 1024
    }

    #[test]
    fn an_aborted_download_resumes_with_a_range_request() {
        let (client, server) = remote_client();
        let root = temp();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();

        let source = remote::location("t1", "big.bin");
        let sides = RemoteSides {
            source: Some(client.clone()),
            dest: None,
        };
        // 第一趟：下到一半叫停。"一半"是按总数说的，具体停在哪个字节
        // 取决于收数据的缓冲有多大——所以不断言死数字，第二趟照实际接上的
        // 那个位置去核对 Range。
        let half = crate::remote::big_bytes().len() as u64 / 2;
        let aborted = copy_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |progress| progress.bytes_done < half,
        );
        assert_eq!(aborted.skipped, 1, "没下完不算完成");
        assert!(
            !dest.join("big.bin").exists(),
            "半截文件不该落在用户的目录里"
        );

        // 断点在暂存文件的实际长度上，别从进度回调猜——回调报的是
        // "这一趟新读了多少"，那是**第一块的大小**（取决于 socket 缓冲），
        // 跟断点不是一回事。猜出来的数偶尔正好等于断点，偶尔不等，
        // 于是这个测试就成了薛定谔的。
        let staging = resume::journal().staging(&resume::key(
            "remote://t1/big.bin",
            &dest.join("big.bin").to_string_lossy(),
        ));
        let resumed_at = fs::metadata(&staging).map(|it| it.len()).unwrap_or(0);
        assert!(
            resumed_at > 0 && resumed_at < crate::remote::big_bytes().len() as u64,
            "该停在半路上：{resumed_at}"
        );
        // 断点必须离末尾还剩至少一块——否则会撞上"叫停落在最后一个字节上
        // 算完成"那条判定（`download_entry` 里 `done >= info.size`），
        // 第一趟本该直接落地而不是中止，整个测试就成了薛定谔的。
        //
        // 块大小是 `std::io::copy` 的默认缓冲（实测 8091/8333…，不是整数 8K），
        // 留一整个缓冲的余量：这个判断不依赖精确块大小，块边界怎么挪都够。
        let total = crate::remote::big_bytes().len() as u64;
        assert!(
            total - resumed_at >= io_copy_buffer(),
            "断点离末尾太近（剩{}字节，不足一个读取缓冲{}），\
             这一趟其实等于下完了",
            total - resumed_at,
            io_copy_buffer()
        );

        // 第二趟：服务器该收到一个带 Range 的请求，位置正是上次停下那儿。
        let outcome = copy_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |_| true,
        );
        assert_eq!(outcome.done, 1);
        assert_eq!(
            fs::read(dest.join("big.bin")).unwrap(),
            crate::remote::big_bytes(),
            "半截接半截，接出来得是原来那一份"
        );
        assert!(
            seen_requests(&server)
                .iter()
                .any(|it| it == &format!("GET /dav/big.bin range={resumed_at}")),
            "续传要走 Range，断点是 {resumed_at}：{:?}",
            seen_requests(&server)
        );
    }

    /// 中止恰好落在最后一个字节上时，那一趟其实**已经下完了**。
    ///
    /// `Reporting` 是读完一块才报进度，所以小块文件必然一次读完、必然在
    /// 文件末尾被判中止。报中止就等于让用户把一份完整的文件留在暂存里
    /// 等着被扫掉——这测试就是钉这个的。
    #[test]
    fn a_download_stopped_on_its_last_byte_still_lands() {
        let (client, _server) = remote_client();
        let root = temp();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let source = remote::location("t1", "small.bin");
        let sides = RemoteSides {
            source: Some(client.clone()),
            dest: None,
        };

        // 只在真收到数据的那一刻叫停。`bytes_done == 0` 的是每个条目
        // 开场先报的那一次"还没开始"，在哪儿停都不算数。
        let outcome = copy_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |progress| progress.bytes_done == 0,
        );

        assert_eq!(outcome.done, 1, "下完了就算完成，不该报中止");
        assert_eq!(outcome.skipped, 0);
        assert_eq!(
            fs::read(dest.join("small.bin")).unwrap(),
            crate::remote::small_bytes(),
            "用户该拿到完整的那一份，不是半截也不是没有"
        );
        // 账本得清掉：这一趟不是"停在中途"，没有什么下次要接着传的。
        let key = resume::key(
            &remote_source(client.as_ref(), "small.bin"),
            &dest.join("small.bin").to_string_lossy(),
        );
        assert!(
            !resume::journal().staging(&key).exists(),
            "暂存该被收走，不能留在盘上等清扫"
        );
    }

    /// 上传侧的同一个洞：`Counting` 数到的字节已经够了，就别再报中止。
    #[test]
    fn an_upload_stopped_on_its_last_byte_still_counts_as_done() {
        let (client, _server) = remote_client();
        let root = temp();
        let local = root.path().join("small.bin");
        fs::write(&local, crate::remote::small_bytes()).unwrap();
        let sides = RemoteSides {
            source: None,
            dest: Some(client.clone()),
        };

        let outcome = copy_entries(
            std::slice::from_ref(&local),
            &remote::location("t1", ""),
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |progress| progress.bytes_done == 0,
        );

        assert_eq!(outcome.done, 1, "传完了就算完成，不该报中止");
        assert_eq!(outcome.skipped, 0);
        let stored = client
            .stat("small.bin")
            .unwrap()
            .expect("服务器上该有这一份");
        assert_eq!(stored.size, crate::remote::small_bytes().len() as u64);
    }

    #[test]
    fn a_download_whose_staging_was_wiped_still_lands() {
        let (client, server) = remote_client();
        let root = temp();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let source = remote::location("t1", "big.bin");
        let sides = RemoteSides {
            source: Some(client.clone()),
            dest: None,
        };

        let aborted = copy_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |progress| progress.bytes_done < 2048,
        );
        assert_eq!(aborted.skipped, 1);

        // 暂存处叫人连窝端了（系统清临时目录、别的程序当垃圾扫了）：
        // 半截数据没了，可服务器还在，重新下一趟就是了。
        let journal = resume::journal();
        let key = resume::key(
            &remote_source(client.as_ref(), "big.bin"),
            &dest.join("big.bin").display().to_string(),
        );
        fs::remove_file(journal.staging(&key)).expect("暂存文件该在");
        journal.clear(&key);

        let mut started_at = None;
        let mut reported = 0u64;
        let outcome = copy_entries(
            std::slice::from_ref(&source),
            &dest,
            ConflictPolicy::KeepBoth,
            &sides,
            &mut |progress| {
                if progress.bytes_done > 0 && started_at.is_none() {
                    started_at = Some(progress.bytes_done);
                }
                reported = progress.bytes_done;
                true
            },
        );

        assert_eq!(outcome.done, 1);
        assert_eq!(
            fs::read(dest.join("big.bin")).unwrap(),
            crate::remote::big_bytes(),
            "没有半截可接也得把文件拿全"
        );
        // "整份重下"要看这一趟一共报了多少字节，而不是看第一块有多大：
        // 第一块的尺寸取决于套接字一次能给多少，那不是断点的事。
        assert_eq!(
            reported,
            crate::remote::big_bytes().len() as u64,
            "没有半截可接，就整份重下"
        );
        assert!(
            !seen_requests(&server)
                .iter()
                .any(|it| it.contains("range=")),
            "这一趟不该发 Range：{:?}",
            seen_requests(&server)
        );
    }
}
