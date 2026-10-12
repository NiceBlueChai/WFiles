//! 文件条目与目录内容模型。
//!
//! 这一层不依赖任何 UI 类型：它只负责读目录、排序、过滤，以及记住哪些路径被选中。

use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// 目录中的一项。
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 预计算的小写名称，避免排序时反复分配。
    sort_name: String,
    /// 预计算的小写扩展名。
    sort_kind: String,
}

impl FileEntry {
    fn from_dir_entry(dir_entry: &fs::DirEntry) -> Option<Self> {
        let path = dir_entry.path();
        let name = path.file_name()?.to_string_lossy().into_owned();
        let metadata = Self::metadata_of(dir_entry, &path)?;
        let sort_kind = if metadata.is_dir() {
            String::new()
        } else {
            path.extension()
                .map(|extension| extension.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        };

        Some(Self {
            sort_name: name.to_lowercase(),
            path,
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
            sort_kind,
        })
    }

    /// `DirEntry::metadata` 在系统已知类型时不会再 stat，但它不跟随符号链接。
    /// 指向文件夹的链接应当表现得像文件夹，所以单独解析；
    /// 失效的链接退回自身元数据，这样它仍然可见、可删除。
    fn metadata_of(dir_entry: &fs::DirEntry, path: &Path) -> Option<fs::Metadata> {
        let file_type = dir_entry.file_type().ok()?;
        if !file_type.is_symlink() {
            return dir_entry.metadata().ok();
        }

        fs::metadata(path)
            .ok()
            .or_else(|| dir_entry.metadata().ok())
    }

    pub fn kind_label(&self) -> String {
        if self.is_dir {
            return "文件夹".to_string();
        }
        if self.sort_kind.is_empty() {
            return "文件".to_string();
        }

        format!("{} 文件", self.sort_kind.to_uppercase())
    }
}

/// 可以排序的列。
///
/// 派生 `Serialize` 是为了让"每个目录记住排序"能落盘。
/// 以后加列时注意：旧配置里没有的字段必须还能读出来。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub struct Sort {
    #[serde(default)]
    pub key: SortKey,
    #[serde(default)]
    pub descending: bool,
}

impl Sort {
    /// 点当前列翻转方向，点新的列从升序重新开始。
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                descending: !self.descending,
            }
        } else {
            Self {
                key,
                descending: false,
            }
        }
    }

    fn compare(self, left: &FileEntry, right: &FileEntry) -> Ordering {
        // 名称是每一列的兜底，保证同样的数据每次排序结果一致。
        let by_name = || left.sort_name.cmp(&right.sort_name);
        let ordering = match self.key {
            SortKey::Name => by_name(),
            SortKey::Size => left.size.cmp(&right.size).then_with(by_name),
            SortKey::Modified => left.modified.cmp(&right.modified).then_with(by_name),
            SortKey::Kind => left.sort_kind.cmp(&right.sort_kind).then_with(by_name),
        };

        if self.descending {
            ordering.reverse()
        } else {
            ordering
        }
    }
}

/// 一个目录的内容，连同它的排序与过滤状态。
///
/// `order` 是排序并过滤后的下标序列，界面只按它渲染，
/// 因此改关键词不需要重新读盘，也只需要渲染可见的那几十行。
#[derive(Debug)]
pub struct Listing {
    path: PathBuf,
    entries: Vec<FileEntry>,
    /// 路径 -> entries 下标。
    index: HashMap<PathBuf, usize>,
    order: Vec<usize>,
    /// entries 下标 -> 当前可见位置，被过滤掉的是 `usize::MAX`。
    rank: Vec<usize>,
    query: String,
    sort: Sort,
}

impl Listing {
    pub fn read(path: &Path) -> Result<Self, String> {
        let read_dir = fs::read_dir(path).map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        for dir_entry in read_dir.flatten() {
            if let Some(entry) = FileEntry::from_dir_entry(&dir_entry) {
                entries.push(entry);
            }
        }

        let index = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| (entry.path.clone(), ix))
            .collect();
        let mut listing = Self {
            path: path.to_path_buf(),
            entries,
            index,
            order: Vec::new(),
            rank: Vec::new(),
            query: String::new(),
            sort: Sort::default(),
        };
        listing.rebuild();
        Ok(listing)
    }

    /// 读盘失败时的占位内容，让界面仍然有一个明确的“当前目录”。
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            index: HashMap::new(),
            order: Vec::new(),
            rank: Vec::new(),
            query: String::new(),
            sort: Sort::default(),
        }
    }

    fn rebuild(&mut self) {
        let entries = &self.entries;
        let sort = self.sort;
        let mut order: Vec<usize> = (0..entries.len()).collect();
        // 文件夹永远排在前面，不论按哪一列、升序还是降序。
        order.sort_by(|&left, &right| {
            entries[right]
                .is_dir
                .cmp(&entries[left].is_dir)
                .then_with(|| sort.compare(&entries[left], &entries[right]))
        });

        let query = self.query.trim().to_lowercase();
        let order = if query.is_empty() {
            order
        } else {
            order
                .into_iter()
                .filter(|&ix| entries[ix].sort_name.contains(&query))
                .collect()
        };

        self.rank = vec![usize::MAX; entries.len()];
        for (position, &ix) in order.iter().enumerate() {
            self.rank[ix] = position;
        }
        self.order = order;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 返回是否真的改变了，调用方据此决定要不要重建行高缓存。
    pub fn set_query(&mut self, query: &str) -> bool {
        if self.query == query {
            return false;
        }

        self.query = query.to_string();
        self.rebuild();
        true
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn set_sort(&mut self, sort: Sort) {
        if self.sort == sort {
            return;
        }

        self.sort = sort;
        self.rebuild();
    }

    pub fn toggle_sort(&mut self, key: SortKey) {
        let sort = self.sort.toggled(key);
        self.set_sort(sort);
    }

    pub fn visible_count(&self) -> usize {
        self.order.len()
    }

    pub fn total_count(&self) -> usize {
        self.entries.len()
    }

    pub fn visible_entry(&self, position: usize) -> Option<&FileEntry> {
        self.order.get(position).map(|&ix| &self.entries[ix])
    }

    pub fn entry(&self, path: &Path) -> Option<&FileEntry> {
        self.index.get(path).and_then(|&ix| self.entries.get(ix))
    }

    /// 路径当前显示在第几行；被过滤掉或不存在时返回 `None`。
    pub fn position_of(&self, path: &Path) -> Option<usize> {
        let ix = *self.index.get(path)?;
        match self.rank.get(ix).copied() {
            Some(position) if position != usize::MAX => Some(position),
            _ => None,
        }
    }
}

/// 当前选中的路径集合。
///
/// 用路径而不是下标做键：刷新或换排序之后，选中状态仍然指向同一批文件。
///
/// `cursor` 是“当前这一项”，方向键和点击都会移动它；
/// `anchor` 是 Shift 连选的起点，只有普通点击和 Command 点击才会移动它，
/// 这样连续按 Shift+方向键才能不断扩大选区。
#[derive(Debug, Default)]
pub struct Selection {
    paths: BTreeSet<PathBuf>,
    cursor: Option<PathBuf>,
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn clear(&mut self) {
        self.paths.clear();
        self.cursor = None;
        self.anchor = None;
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// 恰好选中一项时返回它，用于详情面板和“打开”。
    pub fn single(&self) -> Option<&Path> {
        if self.paths.len() == 1 {
            self.paths.iter().next().map(PathBuf::as_path)
        } else {
            None
        }
    }

    pub fn select(&mut self, path: PathBuf) {
        self.paths.clear();
        self.cursor = Some(path.clone());
        self.anchor = Some(path.clone());
        self.paths.insert(path);
    }

    /// 按住 Command 点击：加进来或拿出去。
    pub fn toggle(&mut self, path: PathBuf) {
        self.cursor = Some(path.clone());
        if !self.paths.remove(&path) {
            self.paths.insert(path.clone());
            self.anchor = Some(path);
        } else if !self
            .anchor
            .as_ref()
            .is_some_and(|anchor| self.paths.contains(anchor))
        {
            self.anchor = self.paths.iter().next().cloned();
        }
    }

    /// 按住 Shift 点击：从锚点连选到目标。
    ///
    /// 锚点保持不变，这样连续几次 Shift 点击可以继续扩大或缩小选区。
    pub fn select_range(&mut self, listing: &Listing, path: PathBuf) {
        let target = listing.position_of(&path);
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|anchor| listing.position_of(anchor));
        let (Some(anchor), Some(target)) = (anchor, target) else {
            self.select(path);
            return;
        };

        self.cursor = Some(path);
        let (start, end) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        self.paths.clear();
        for position in start..=end {
            if let Some(entry) = listing.visible_entry(position) {
                self.paths.insert(entry.path.clone());
            }
        }
    }

    pub fn select_all(&mut self, listing: &Listing) {
        self.paths.clear();
        for position in 0..listing.visible_count() {
            if let Some(entry) = listing.visible_entry(position) {
                self.paths.insert(entry.path.clone());
            }
        }
        let first = listing.visible_entry(0).map(|entry| entry.path.clone());
        self.cursor = first.clone();
        self.anchor = first;
    }

    /// 按列表顺序返回选中项，批量操作要用同一个顺序。
    pub fn ordered(&self, listing: &Listing) -> Vec<PathBuf> {
        (0..listing.visible_count())
            .filter_map(|position| listing.visible_entry(position))
            .map(|entry| entry.path.clone())
            .filter(|path| self.paths.contains(path))
            .collect()
    }

    /// 键盘导航的起点：光标当前所在的行。
    ///
    /// 与 [`Selection::cursor`] 不同，这里不要求该项仍被选中——
    /// Command 点击取消选中之后，方向键还是要从那一行接着走。
    pub fn focus_position(&self, listing: &Listing) -> Option<usize> {
        self.cursor
            .as_ref()
            .or(self.anchor.as_ref())
            .and_then(|path| listing.position_of(path))
    }

    /// 回车要打开的那一项：多选时也是光标所在的那一项。
    ///
    /// 光标可能停在一个已经被 Command 点击取消选中的项上，这时返回 `None`。
    pub fn cursor(&self) -> Option<&Path> {
        self.cursor
            .as_deref()
            .filter(|cursor| self.paths.contains(*cursor))
    }

    /// 目录刷新后丢掉已经不存在的路径。
    pub fn retain_existing(&mut self, listing: &Listing) {
        let before = self.paths.len();
        self.paths.retain(|path| listing.entry(path).is_some());

        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| !self.paths.contains(cursor))
        {
            self.cursor = self.paths.iter().next().cloned();
        }

        if self.paths.len() != before
            && self
                .anchor
                .as_ref()
                .is_none_or(|anchor| !self.paths.contains(anchor))
        {
            self.anchor = self.paths.iter().next().cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing_of(dir: &Path) -> Listing {
        Listing::read(dir).expect("read temp dir")
    }

    fn names(listing: &Listing) -> Vec<String> {
        (0..listing.visible_count())
            .filter_map(|ix| listing.visible_entry(ix))
            .map(|entry| entry.name.clone())
            .collect()
    }

    #[test]
    fn directories_sort_before_files() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("aaa.txt"), "x").unwrap();
        fs::create_dir(root.path().join("zzz")).unwrap();

        assert_eq!(names(&listing_of(root.path())), ["zzz", "aaa.txt"]);
    }

    #[test]
    fn names_sort_case_insensitively() {
        let root = tempfile::tempdir().unwrap();
        for name in ["Beta", "alpha", "Gamma"] {
            fs::write(root.path().join(name), "x").unwrap();
        }

        assert_eq!(names(&listing_of(root.path())), ["alpha", "Beta", "Gamma"]);
    }

    #[test]
    fn directories_stay_first_when_descending() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("aaa_dir")).unwrap();
        fs::write(root.path().join("mmm.txt"), "x").unwrap();
        fs::write(root.path().join("zzz.txt"), "x").unwrap();

        let mut listing = listing_of(root.path());
        listing.toggle_sort(SortKey::Name);

        assert_eq!(
            names(&listing),
            ["aaa_dir", "zzz.txt", "mmm.txt"],
            "降序只作用于同类之间，文件夹仍然在最前"
        );
        assert!(listing.sort().descending);
    }

    #[test]
    fn sorting_by_size_keeps_names_as_tiebreaker() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("big.txt"), "xxxx").unwrap();
        fs::write(root.path().join("small.txt"), "x").unwrap();
        fs::write(root.path().join("tie_b.txt"), "xx").unwrap();
        fs::write(root.path().join("tie_a.txt"), "xx").unwrap();

        let mut listing = listing_of(root.path());
        listing.toggle_sort(SortKey::Size);
        assert_eq!(
            names(&listing),
            ["small.txt", "tie_a.txt", "tie_b.txt", "big.txt"]
        );

        listing.toggle_sort(SortKey::Size);
        assert_eq!(
            names(&listing),
            ["big.txt", "tie_b.txt", "tie_a.txt", "small.txt"]
        );
    }

    #[test]
    fn sorting_by_kind_groups_extensions() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("a.txt"), "x").unwrap();
        fs::write(root.path().join("b.log"), "x").unwrap();
        fs::write(root.path().join("c.txt"), "x").unwrap();

        let mut listing = listing_of(root.path());
        listing.toggle_sort(SortKey::Kind);

        assert_eq!(names(&listing), ["dir", "b.log", "a.txt", "c.txt"]);
    }

    #[test]
    fn clicking_a_new_column_restarts_ascending() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "x").unwrap();

        let mut listing = listing_of(root.path());
        assert_eq!(listing.sort(), Sort::default());

        listing.toggle_sort(SortKey::Name);
        assert!(
            listing.sort().descending,
            "默认就是名称升序，点一下应当翻转"
        );

        listing.toggle_sort(SortKey::Modified);
        assert_eq!(listing.sort().key, SortKey::Modified);
        assert!(!listing.sort().descending);
    }

    #[test]
    fn filter_narrows_the_visible_order() {
        let root = tempfile::tempdir().unwrap();
        let alpha = root.path().join("alpha.txt");
        let beta = root.path().join("beta.log");
        fs::write(&alpha, "x").unwrap();
        fs::write(&beta, "x").unwrap();

        let mut listing = listing_of(root.path());
        assert_eq!(listing.visible_count(), 2);
        assert_eq!(listing.position_of(&alpha), Some(0));
        assert_eq!(listing.position_of(&beta), Some(1));

        listing.set_query("beta");
        assert_eq!(names(&listing), ["beta.log"]);
        assert_eq!(listing.total_count(), 2);
        assert_eq!(listing.position_of(&beta), Some(0));
        assert_eq!(listing.position_of(&alpha), None);

        listing.set_query("");
        assert_eq!(listing.visible_count(), 2);
        assert_eq!(listing.position_of(&alpha), Some(0));
    }

    #[test]
    fn shift_click_selects_the_range_between_anchor_and_target() {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt", "d.txt"]
            .iter()
            .map(|name| root.path().join(name))
            .collect();
        for path in &paths {
            fs::write(path, "x").unwrap();
        }

        let listing = listing_of(root.path());
        let mut selection = Selection::default();
        selection.select(paths[1].clone());
        selection.select_range(&listing, paths[3].clone());

        assert_eq!(selection.len(), 3);
        assert_eq!(selection.ordered(&listing), paths[1..4]);
        assert_eq!(selection.cursor(), Some(paths[3].as_path()));
        assert_eq!(selection.focus_position(&listing), Some(3));

        // 往回选也要能收缩，锚点留在第一次点击的地方。
        selection.select_range(&listing, paths[0].clone());
        assert_eq!(selection.ordered(&listing), paths[0..2]);
        assert_eq!(selection.focus_position(&listing), Some(0));
    }

    #[test]
    fn shift_click_without_anchor_selects_only_the_target() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("a.txt");
        let second = root.path().join("b.txt");
        fs::write(&first, "x").unwrap();
        fs::write(&second, "x").unwrap();

        let listing = listing_of(root.path());
        let mut selection = Selection::default();
        selection.select_range(&listing, second.clone());

        assert_eq!(selection.len(), 1);
        assert_eq!(selection.cursor(), Some(second.as_path()));

        // 第一次 Shift 点击也立下了锚点，所以再往回连选是有效的。
        selection.select_range(&listing, first.clone());
        assert_eq!(selection.ordered(&listing), [first, second]);
    }

    #[test]
    fn toggle_removes_and_readds() {
        let root = tempfile::tempdir().unwrap();
        let alpha = root.path().join("alpha.txt");
        let beta = root.path().join("beta.txt");
        fs::write(&alpha, "x").unwrap();
        fs::write(&beta, "x").unwrap();

        let listing = listing_of(root.path());
        let mut selection = Selection::default();
        selection.toggle(alpha.clone());
        selection.toggle(beta.clone());
        assert_eq!(selection.len(), 2);

        selection.toggle(beta.clone());
        assert_eq!(selection.len(), 1);
        assert!(selection.contains(&alpha));
        assert_eq!(selection.cursor(), None, "光标停在刚被取消选中的那一项上");

        selection.toggle(alpha.clone());
        assert!(selection.is_empty());
        assert_eq!(
            selection.focus_position(&listing),
            Some(0),
            "全部取消选中之后，方向键还是从最后点过的地方接着走"
        );
    }

    #[test]
    fn cmd_click_moves_the_cursor_without_touching_the_other_selections() {
        let root = tempfile::tempdir().unwrap();
        let alpha = root.path().join("alpha.txt");
        let gamma = root.path().join("gamma.txt");
        fs::write(&alpha, "x").unwrap();
        fs::write(root.path().join("beta.txt"), "x").unwrap();
        fs::write(&gamma, "x").unwrap();

        let listing = listing_of(root.path());
        let mut selection = Selection::default();
        selection.select(alpha.clone());
        selection.toggle(gamma.clone());

        assert_eq!(selection.ordered(&listing), [alpha, gamma.clone()]);
        assert_eq!(
            selection.focus_position(&listing),
            Some(2),
            "方向键要从最后点过的那一项接着走"
        );
    }

    #[test]
    fn select_all_follows_the_visible_order() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("alpha.txt"), "x").unwrap();
        fs::write(root.path().join("beta.log"), "x").unwrap();
        fs::write(root.path().join("gamma.txt"), "x").unwrap();

        let mut listing = listing_of(root.path());
        listing.set_query(".txt");

        let mut selection = Selection::default();
        selection.select_all(&listing);

        assert_eq!(selection.len(), 2);
        assert_eq!(
            selection.ordered(&listing),
            [root.path().join("alpha.txt"), root.path().join("gamma.txt")]
        );
    }

    #[test]
    fn refresh_drops_selection_for_deleted_files() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("keep.txt"), "x").unwrap();
        fs::write(root.path().join("gone.txt"), "x").unwrap();

        let mut selection = Selection::default();
        selection.select(root.path().join("gone.txt"));
        assert_eq!(selection.len(), 1);

        fs::remove_file(root.path().join("gone.txt")).unwrap();
        selection.retain_existing(&listing_of(root.path()));

        assert_eq!(selection.len(), 0);
        assert_eq!(selection.single(), None);
    }

    #[test]
    fn symlink_to_directory_reports_as_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("real");
        fs::create_dir(&target).unwrap();

        let link = root.path().join("link");
        if std::os::unix::fs::symlink(&target, &link).is_err() {
            return;
        }

        let listing = listing_of(root.path());
        assert!(listing.entry(&link).is_some_and(|entry| entry.is_dir));
    }

    #[test]
    fn broken_symlink_stays_listed() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("broken");
        if std::os::unix::fs::symlink(root.path().join("missing"), &link).is_err() {
            return;
        }

        assert!(listing_of(root.path()).entry(&link).is_some());
    }

    #[test]
    fn unreadable_directory_falls_back_to_an_empty_listing() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");

        assert!(Listing::read(&missing).is_err());

        let listing = Listing::empty(missing.clone());
        assert_eq!(listing.path(), missing.as_path());
        assert_eq!(listing.visible_count(), 0);
        assert_eq!(listing.position_of(&missing), None);
    }
}
