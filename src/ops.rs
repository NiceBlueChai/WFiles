//! 文件操作：计划、执行、冲突处理与撤销。
//!
//! 这一层不依赖 UI：它只回答“做什么、按什么顺序做、做错了怎么退回”。
//! 所有函数都是同步的，由调用方放到后台线程执行，通过 [`Progress`] 回调汇报进度。

use std::{
    fs,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

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

/// 一次操作的进度快照，界面据此画进度条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// 当前动作的人类可读描述，例如“正在拷贝 report.pdf”。
    pub label: String,
    pub done: usize,
    pub total: usize,
}

/// 一次操作的结果：成功多少、跳过多少、哪些失败了。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub done: usize,
    pub skipped: usize,
    pub failed: Vec<(PathBuf, String)>,
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
pub fn create_folder_named(parent: &Path, name: &str) -> Result<PathBuf, String> {
    valid_name(name)?;
    let trimmed = name.trim();
    let path = parent.join(trimmed);
    if path.exists() {
        return Err(format!("“{trimmed}”已存在"));
    }
    fs::create_dir(&path).map_err(|error| format!("无法创建文件夹：{error}"))?;
    Ok(path)
}

/// 重命名。只改名字，不换目录。
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    valid_name(new_name)?;
    let trimmed = new_name.trim();

    let parent = path
        .parent()
        .ok_or_else(|| "没有父目录，无法重命名".to_string())?;
    let target = parent.join(trimmed);
    if target.exists() && !same_file(path, &target) {
        return Err(format!("“{trimmed}”已存在"));
    }

    fs::rename(path, &target).map_err(|error| format!("重命名失败：{error}"))?;
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
pub fn restore_from_trash(original: &Path, trashed_at: SystemTime) -> Result<(), String> {
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
        let items = trash::os_limited::list().map_err(|error| error.to_string())?;
        let mut matches: Vec<_> = items
            .into_iter()
            .filter(|item| item.original_path() == original)
            .collect();
        // 时间戳越大删除越晚，放回最新的那条。
        matches.sort_by_key(|item| item.time_deleted);
        let Some(item) = matches.pop() else {
            return Err("回收站里找不到这一项".to_string());
        };
        return trash::os_limited::restore_all([item]).map_err(|error| error.to_string());
    }

    #[cfg(target_os = "macos")]
    {
        let trash_dir = dirs::home_dir()
            .ok_or_else(|| "找不到个人文件夹，无法定位回收站".to_string())?
            .join(".Trash");
        restore_from_trash_dir(&trash_dir, original, trashed_at)
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        all(
            unix,
            not(target_os = "ios"),
            not(target_os = "android")
        )
    )))]
    {
        let _ = (original, trashed_at);
        Err("这个平台不支持从回收站撤销，请手动打开回收站放回".to_string())
    }
}

/// macOS 的回收站恢复实现，回收站目录由调用方给出（测试里用临时目录）。
#[cfg(target_os = "macos")]
fn restore_from_trash_dir(
    trash_dir: &Path,
    original: &Path,
    trashed_at: SystemTime,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    let name = original
        .file_name()
        .ok_or_else(|| "无法解析原文件名".to_string())?
        .to_string_lossy()
        .into_owned();
    let (stem, extension) = split_name(&name);

    // 留 2 秒余量，吸收“删除动作先记时刻、系统后落盘”的时钟误差。
    let cutoff = trashed_at - Duration::from_secs(2);
    let mut best: Option<(i64, PathBuf)> = None;

    let entries = fs::read_dir(trash_dir).map_err(|error| error.to_string())?;
    for entry in entries.flatten() {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        if !trash_name_matches(&entry_name, &stem, &extension) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else { continue };
        // 移入回收站会刷新 inode 的 ctime；早于删除时刻的是更早删的，不动。
        if metadata.ctime() < cutoff_seconds(cutoff) {
            continue;
        }
        if best.as_ref().is_none_or(|(seen, _)| metadata.ctime() > *seen) {
            best = Some((metadata.ctime(), entry.path()));
        }
    }

    let (_, candidate) = best.ok_or_else(|| {
        "回收站里找不到这次删掉的项，请手动打开回收站放回".to_string()
    })?;

    if original.exists() {
        return Err("原位置已经有同名项目，先移走它再撤销".to_string());
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::rename(&candidate, original).map_err(|error| error.to_string())
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
fn copy_entry(source: &Path, dest: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(source, dest).map(|_| ())
    }
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
/// `on_progress` 每完成一项调用一次，返回 false 表示用户要求中止。
pub fn copy_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    transfer_entries(sources, dest_dir, policy, false, on_progress)
}

/// 把一组条目移进 `dest_dir`；跨卷时按“复制成功后删源”处理。
pub fn move_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    transfer_entries(sources, dest_dir, policy, true, on_progress)
}

fn transfer_entries(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    remove_source: bool,
    on_progress: &mut dyn FnMut(&Progress) -> bool,
) -> Outcome {
    let total = sources.len();
    let mut outcome = Outcome::default();

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
            label,
            done: ix,
            total,
        }) {
            outcome.skipped += total - ix;
            return outcome;
        }

        let target = dest_dir.join(&name);
        // 往自己的父目录移动自己是无意义的，也不该判成冲突。
        let same_dir = source.parent() == Some(dest_dir);
        let conflict = target.exists() && !same_file(source, &target) && !same_dir;

        let Some(target) = (if conflict {
            resolve_conflict(&target, policy)
        } else {
            Some(target)
        }) else {
            outcome.skipped += 1;
            continue;
        };

        // 目标是移动源的子目录时会形成环，直接拒绝。
        if remove_source && target.starts_with(source) {
            outcome
                .failed
                .push((source.clone(), "不能把文件夹移进它自己".to_string()));
            continue;
        }

        match transfer_one(source, &target, remove_source) {
            Ok(()) => {
                outcome.done += 1;
                outcome.touched.push(target.clone());
                if remove_source {
                    outcome.transfers.push((source.clone(), target));
                }
            }
            Err(error) => outcome.failed.push((source.clone(), error.to_string())),
        }
    }

    on_progress(&Progress {
        label: "完成".to_string(),
        done: total,
        total,
    });
    outcome
}

fn transfer_one(source: &Path, target: &Path, remove_source: bool) -> Result<(), String> {
    if remove_source {
        // 同卷移动一步到位；跨卷先拷贝再删源。
        if fs::rename(source, target).is_ok() {
            return Ok(());
        }
    }

    copy_entry(source, target).map_err(|error| format!("拷贝失败：{error}"))?;

    if remove_source {
        remove_entry(source).map_err(|error| format!("已拷贝但删除源文件失败：{error}"))?;
    }
    Ok(())
}

/// 拷贝/移动前先数一遍总条目数，让进度条从一开始就有正确的分母。
pub fn count_entries(paths: &[PathBuf]) -> usize {
    paths.iter().map(|path| count_one(path)).sum()
}

fn count_one(path: &Path) -> usize {
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
        .map(|entry| count_one(&entry.path()))
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
    /// 移进回收站：撤销就是从回收站放回原处。
    Trashed { originals: Vec<(PathBuf, SystemTime)> },
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
            Self::Trashed { originals } => {
                originals.iter().map(|(path, _)| path.clone()).collect()
            }
        }
    }

    /// 撤销按钮上的文案。
    pub fn describe(&self) -> String {
        match self {
            Self::Created { path } => {
                format!("新建“{}”", display_name(path))
            }
            Self::Renamed { from, to } => {
                format!("重命名“{}”为“{}”", display_name(from), display_name(to))
            }
            Self::Copied { created } => {
                format!("复制 {} 项", created.len())
            }
            Self::Moved { moves } => {
                format!("移动 {} 项", moves.len())
            }
            Self::Trashed { originals } => {
                format!("移到回收站 {} 项", originals.len())
            }
        }
    }

    /// 执行撤销。失败时返回原因，且这一条记录仍然留在栈里（情况没变，重试也许还有意义）。
    pub fn undo(&self) -> Result<(), String> {
        match self {
            Self::Created { path } => {
                let metadata = fs::symlink_metadata(path)
                    .map_err(|_| "这一项已经不存在了".to_string())?;
                if metadata.is_dir() {
                    let empty = fs::read_dir(path)
                        .map(|mut entries| entries.next().is_none())
                        .unwrap_or(false);
                    if !empty {
                        return Err("文件夹里已经有了新内容，先清空再撤销".to_string());
                    }
                }
                remove_entry(path).map_err(|error| error.to_string())
            }
            Self::Renamed { from, to } => {
                if !to.exists() {
                    return Err("原文件已经不在改名后的位置了".to_string());
                }
                fs::rename(to, from).map_err(|error| error.to_string())
            }
            Self::Copied { created } => {
                for path in created {
                    if path.exists() {
                        remove_entry(path).map_err(|error| error.to_string())?;
                    }
                }
                Ok(())
            }
            Self::Moved { moves } => {
                for (from, to) in moves.iter().rev() {
                    if let Some(parent) = from.parent() {
                        fs::create_dir_all(parent).ok();
                    }
                    fs::rename(to, from).map_err(|error| error.to_string())?;
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
    pub fn last_describe(&self) -> Option<String> {
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
    pub fn pop_undo(&mut self) -> Result<String, String> {
        let entry = self
            .entries
            .pop()
            .ok_or_else(|| "没有可撤销的操作".to_string())?;
        match entry.undo() {
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
            HistoryEntry::Trashed { originals } => {
                originals.retain(|(item, _)| item != path);
                !originals.is_empty()
            }
        });
    }
}

/// 名字是否合法（新建/重命名共用）。
pub fn valid_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名称不能为空".to_string());
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\\') {
        return Err("名称不能包含路径分隔符".to_string());
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
        assert_eq!(resolve_conflict(&target, ConflictPolicy::Replace), Some(target.clone()));
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
            &[src.clone()],
            &src.join("inner"),
            ConflictPolicy::KeepBoth,
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

        let mut seen = 0;
        let outcome = copy_entries(
            &(0..6)
                .map(|ix| src.join(format!("f{ix}.txt")))
                .collect::<Vec<_>>(),
            &dest,
            ConflictPolicy::KeepBoth,
            &mut |_| {
                seen += 1;
                seen < 3
            },
        );

        assert_eq!(outcome.done, 2, "第 3 项开始前被叫停");
        assert_eq!(outcome.skipped, 4, "剩下的全部记为跳过");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
    }

    #[test]
    fn count_entries_counts_recursively() {
        let root = temp();
        let src = root.path().join("src");
        fs::create_dir_all(src.join("nested/deep")).unwrap();
        fs::write(src.join("a.txt"), "x").unwrap();
        fs::write(src.join("nested/b.txt"), "x").unwrap();
        fs::write(src.join("nested/deep/c.txt"), "x").unwrap();

        assert_eq!(count_entries(&[src.clone()]), 6);
        assert_eq!(count_entries(&[src.join("a.txt"), src]), 7);
    }

    // ---- 撤销 ----

    #[test]
    fn undo_create_removes_the_folder() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created { path: folder.clone() });
        assert_eq!(
            history.last_describe(),
            Some("新建“未命名文件夹”".to_string())
        );

        assert_eq!(history.pop_undo().unwrap(), "新建“未命名文件夹”");
        assert!(!folder.exists());
        assert!(history.is_empty());
    }

    #[test]
    fn undo_create_refuses_a_folder_that_gained_content() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();
        fs::write(folder.join("later.txt"), "x").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created { path: folder.clone() });

        assert!(history.pop_undo().is_err());
        assert!(
            history.last_describe().is_some(),
            "撤不动要留在栈里"
        );
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

        history.pop_undo().unwrap();
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
            &mut |_| true,
        );

        let mut history = History::default();
        history.push(HistoryEntry::Copied {
            created: outcome.touched.clone(),
        });
        history.pop_undo().unwrap();

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
        let outcome = move_entries(&sources, &dest, ConflictPolicy::KeepBoth, &mut |_| true);
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

        history.pop_undo().unwrap();
        assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(src.join("b.txt")).unwrap(), "bbb");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn note_renamed_keeps_undo_pointing_at_the_new_name() {
        let root = temp();
        let folder = create_folder_named(root.path(), "未命名文件夹").unwrap();
        let renamed = rename(&folder, "改名了").unwrap();

        let mut history = History::default();
        history.push(HistoryEntry::Created { path: folder.clone() });
        history.note_renamed(&folder, &renamed);

        assert_eq!(
            history.entries[0],
            HistoryEntry::Created { path: renamed.clone() }
        );
        history.pop_undo().unwrap();
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
            history.last_describe(),
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

        assert!(error.contains("回收站"), "应当给出可理解的失败原因：{error}");
        assert!(trash_dir.join("old.txt").exists(), "旧项不能被误放回");
    }
}
