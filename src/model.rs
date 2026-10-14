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
    /// 这项是不是隐藏项（文件名以 `.` 开头）。
    ///
    /// **判断在读盘时做一次，不在渲染时算**：`Listing::refilter`
    /// 每敲一个过滤字母就要扫一遍全部条目，而 `starts_with('.')`
    /// 虽然便宜，十 万次字符串切片在一帧里仍能感觉到。
    hidden: bool,
}

impl FileEntry {
    /// 名字以 `.` 开头就算隐藏项。
    ///
    /// **三个平台同一把尺子，没有 `#[cfg]`**。Windows 上确实有
    /// 「隐藏属性」（`attrib +h`），但那个属性绝大多数用户从没用过，
    /// 而 `.git` / `.env` / `node_modules` 这些是天天要看的——
    /// 按属性判的话，Windows 用户按了快捷键还是什么都看不到，
    /// 那才是真的坏。所以一律按名字判。
    ///
    /// `.` 与 `..` 是目录的两条内建入口，不在 `read_dir` 的结果里，
    /// 碰不到这里（万一真进来了，`name` 就是这两个之一，
    /// 会被当成隐藏项——那种情况下它本来也不该作为一个文件出现）。
    fn is_hidden_name(name: &str) -> bool {
        name.starts_with('.')
    }

    /// 只给测试用：造一批条目，不碰磁盘。
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, name: String, is_dir: bool, size: u64) -> Self {
        let sort_name = name.to_lowercase();
        let sort_kind = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        Self {
            path,
            hidden: Self::is_hidden_name(&name),
            name,
            is_dir,
            size,
            modified: None,
            sort_name,
            sort_kind,
        }
    }

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
            hidden: Self::is_hidden_name(&name),
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

    /// 远程目录里的一项。字段由调用方给——只有它知道怎么从协议里读出来。
    ///
    /// 放在这里而不是 `remote` 里，是为了让"排序、过滤、显示"这一套
    /// 不用管东西是本地的还是远端的。
    pub fn remote(
        path: PathBuf,
        name: &str,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Self {
        Self {
            sort_name: name.to_lowercase(),
            sort_kind: if is_dir {
                String::new()
            } else {
                path.extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
            },
            hidden: Self::is_hidden_name(name),
            path,
            name: name.to_string(),
            is_dir,
            size,
            modified,
        }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// 窗格用哪种方式显示当前目录。
///
/// 派生 `Serialize` 是为了"每个目录记住它的视图"能落盘；
/// 和 [`Sort`] 一样，旧配置里没有这个字段时必须还能读出来。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// 一行一项，带大小、修改时间、类型三列。
    #[default]
    List,
    /// 缩略图格子：看图挑图时一行一项太浪费。
    Grid,
    /// Miller 分列：从根目录一级一列铺下来，选定哪一层就展开下一层。
    Columns,
}

impl ViewMode {
    /// 工具栏和命令面板上怎么写。
    pub fn label(self) -> &'static str {
        match self {
            Self::List => "列表",
            Self::Grid => "网格",
            Self::Columns => "分列",
        }
    }

    /// 快捷键提示里用的那一位数字。
    pub fn shortcut(self) -> &'static str {
        match self {
            Self::List => "Cmd+1",
            Self::Grid => "Cmd+2",
            Self::Columns => "Cmd+3",
        }
    }

    /// 所有视图，按工具栏上的排列顺序。
    pub const ALL: [Self; 3] = [Self::List, Self::Grid, Self::Columns];
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
    /// 按排序键排好、**但还没过滤**的全序。
    ///
    /// 留着它，过滤框清空时不必再排一次：排序键没变过，重排出来的
    /// 结果跟原来一模一样，纯属白花 10 万次比较。
    sorted: Vec<usize>,
    /// entries 下标 -> 当前可见位置，被过滤掉的是 `usize::MAX`。
    rank: Vec<usize>,
    query: String,
    sort: Sort,
    /// 要不要显示隐藏项（见 [`FileEntry::hidden`]）。
    show_hidden: bool,
}

impl Listing {
    /// 读一个本地目录。
    ///
    /// `show_hidden` 由调用方给（它读的是 `Settings`）：这一层
    /// 不该知道设置存在哪儿，`Settings` 是 GPUI 的全局槽位，
    /// 而这一层刻意不依赖任何 UI 类型。
    pub fn read(path: &Path, show_hidden: bool) -> Result<Self, crate::i18n::Msg> {
        let read_dir = fs::read_dir(path).map_err(crate::i18n::Message::from)?;
        let mut entries = Vec::new();
        for dir_entry in read_dir.flatten() {
            if let Some(entry) = FileEntry::from_dir_entry(&dir_entry) {
                entries.push(entry);
            }
        }
        // **用户正在看的这个目录自己是隐藏的，就把它里面的东西全显示出来。**
        // 不这么做的话，从命令行 `cd ~/.config` 进来会看到一个空目录——
        // 而那个空目录只是"所有内容都被过滤掉了"，
        // 用户会以为 `.config` 是空的。这条跟 Finder / 资源管理器一致。
        let show_hidden = show_hidden || Self::path_is_hidden(path);
        Ok(Self::from_entries_showing(
            path.to_path_buf(),
            entries,
            show_hidden,
        ))
    }

    /// 这个**目录自己**是不是隐藏目录。
    ///
    /// 只看 `file_name()` 的第一个字符，不看整条路径——否则 `~/.config/foo`
    /// 这种深路径下的普通目录也会被判成隐藏，`~` 底下的 `.ssh`、`.aws`
    /// 会在用户关着开关时冒出来。
    ///
    /// **`.` 与 `..` 也算，但它们到不了这里**：`fs::read_dir` 的结果里
    /// 没有这两条（那是目录的入口，不是目录里的项）。
    fn path_is_hidden(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(FileEntry::is_hidden_name)
    }

    /// 把一批条目装成目录内容。
    ///
    /// 远程目录走的就是这条路：那一批条目是后台线程从服务器上取回来的，
    /// 之后排序、过滤、选中与本地目录一模一样。
    ///
    /// `show_hidden` 一起收进来而不是让调用方在构造之后
    /// `set_show_hidden`：那个方法走 `refilter`，而这里紧接着
    /// 就要排一次序——分开写的话中间那一瞬的可见列表是错的，
    /// 而那种错只在"先建再筛"这一条路上出现，测试很难碰到。
    pub fn from_entries_showing(path: PathBuf, entries: Vec<FileEntry>, show_hidden: bool) -> Self {
        let index = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| (entry.path.clone(), ix))
            .collect();
        let mut listing = Self {
            path,
            entries,
            index,
            order: Vec::new(),
            sorted: Vec::new(),
            rank: Vec::new(),
            query: String::new(),
            sort: Sort::default(),
            show_hidden,
        };
        listing.resort();
        listing
    }

    /// 读盘失败时的占位内容，让界面仍然有一个明确的“当前目录”。
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            index: HashMap::new(),
            order: Vec::new(),
            sorted: Vec::new(),
            rank: Vec::new(),
            query: String::new(),
            sort: Sort::default(),
            show_hidden: false,
        }
    }

    /// 按当前排序键把全部条目排一遍（过滤条件不变，顺序变了就整体重排）。
    fn resort(&mut self) {
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
        self.sorted = order;
        self.refilter();
    }

    /// 只在**已经排好**的顺序上按当前查询筛一遍，不重排。
    ///
    /// 为什么过滤不该重新排序：排序键一个字符都没变，重排出来的结果
    /// 跟原来一模一样——10 万项的目录里光这一下就是十几毫秒，而过滤框
    /// 每敲一个字母都要走一次（实测 19 ms/次，直接吃掉一帧）。
    /// 在既有顺序上筛只是线性扫描，代价是排序的一个零头。
    ///
    /// `self.sorted` 留着**没筛过**的全序：过滤框清空时不必再排一次。
    ///
    /// **隐藏项在这里一起筛掉**（第二个条件），不单独一趟：
    /// 两个条件都是"在既有顺序上按一个布尔筛"，分成两趟要多扫一遍
    /// 10 万个 `usize`，而合成一个 `.filter()` 让分支预测器也省一次。
    fn refilter(&mut self) {
        let entries = &self.entries;
        let query = self.query.trim().to_lowercase();
        let show_hidden = self.show_hidden;
        let order: Vec<usize> = self
            .sorted
            .iter()
            .copied()
            .filter(|&ix| show_hidden || !entries[ix].hidden)
            .filter(|&ix| query.is_empty() || entries[ix].sort_name.contains(&query))
            .collect();
        // `rank` 的重建只占 0.4 ms（10 万个 `usize` 的 memset），
        // 而真瓶颈是上面那趟字符串搜索（7.4 ms）。为 0.4 ms 把
        // `position_of` 改成 `&mut self` 要动 11 处调用点，不划算——
        // 实测过了，撤掉。
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
        // 只重筛，不重排：排序键一个字符都没变。10 万项重排一次
        // 实测 19 ms，过滤框每敲一个字母都要付这笔钱。
        self.refilter();
        true
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// 现在显不显示隐藏项。
    pub fn shows_hidden(&self) -> bool {
        self.show_hidden
    }

    /// 切换隐藏项。返回是否真的改了，调用方据此决定要不要重建行高缓存。
    ///
    /// **只重筛，不重读盘**：`entries` 里那些项一直都在，
    /// 只是刚才没进 `order`。所以这个开关在 10 万项目录里也是
    /// 一次线性扫描（实测 7 ms 量级），而不是重新读一遍目录。
    ///
    /// 连带的好处：状态栏那句「N / M 个项目」里的 `M`
    /// 走的是 [`Self::total_count`]（磁盘上真实总数），
    /// 所以按下开关能**看见** M 变不变——把隐藏项在读盘时就丢掉的话，
    /// 两个数字永远相等，用户会以为开关没生效。
    pub fn set_show_hidden(&mut self, show: bool) -> bool {
        if self.show_hidden == show {
            return false;
        }
        self.show_hidden = show;
        self.refilter();
        true
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn set_sort(&mut self, sort: Sort) {
        if self.sort == sort {
            return;
        }

        self.sort = sort;
        self.resort();
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

    /// 一次把整批选中项装回去（崩溃恢复用）。
    ///
    /// 不能拿 [`Selection::select`] 循环——那个每次都 `clear()`，
    /// 循环 N 次就只剩最后一个了。
    ///
    /// 光标与锚点落在 `cursor` 上：有就落在它（键盘接着从那儿走），
    /// 没有就落在最后一个（列表最靠下的那一项，按方向键往上走最自然）。
    pub fn select_many(&mut self, paths: Vec<PathBuf>, cursor: Option<PathBuf>) {
        self.paths.clear();
        self.paths.extend(paths.iter().cloned());
        let cursor = cursor
            .filter(|path| self.paths.contains(path))
            .or_else(|| paths.last().cloned());
        self.cursor = cursor.clone();
        self.anchor = cursor;
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
        // 隐藏项默认不显示，跟真实设置一致（`Settings::default` 那个 false）。
        // 要看隐藏项的测试走 [`Listing::set_show_hidden`]。
        Listing::read(dir, false).expect("read temp dir")
    }

    fn names(listing: &Listing) -> Vec<String> {
        (0..listing.visible_count())
            .filter_map(|ix| listing.visible_entry(ix))
            .map(|entry| entry.name.clone())
            .collect()
    }

    #[test]
    fn every_view_has_a_label_and_a_shortcut() {
        for mode in ViewMode::ALL {
            assert!(!mode.label().is_empty());
            assert!(mode.shortcut().starts_with("Cmd+"));
        }
    }

    #[test]
    fn a_view_mode_survives_the_roundtrip() {
        for mode in ViewMode::ALL {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<ViewMode>(&json).unwrap(), mode);
        }
        // 旧配置里没有这个字段：读出默认值而不是报错。
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default)]
            view: ViewMode,
        }
        assert_eq!(
            serde_json::from_str::<Holder>("{}").unwrap().view,
            ViewMode::List
        );
    }

    /// 造一个带隐藏项的目录：`.env` 一个文件、`.config` 一个目录，
    /// 外加两个普通项。文件夹排在前面，所以期望值是有序的。
    ///
    /// **返回 `path` 而不是那个 `TempDir`**：macOS 上 `tempfile::tempdir()`
    /// 给的目录名是 `.tmpXXXX`（以点开头），而 [`Listing::read`] 里那条
    /// 「当前目录是隐藏的就全显示」会把它当成隐藏目录——
    /// 于是每个用这个夹具的测试都会莫名其妙地"看得见隐藏项"。
    /// 所以真正被读的那个目录放在一层真名字的子目录底下。
    fn dir_with_hidden_items() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plain");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("aaa.txt"), "x").unwrap();
        fs::write(dir.join(".env"), "x").unwrap();
        fs::write(dir.join("zzz.txt"), "x").unwrap();
        fs::create_dir(dir.join(".config")).unwrap();
        (root, dir)
    }

    #[test]
    fn hidden_items_are_out_of_the_way_until_you_ask_for_them() {
        let (_keep, dir) = dir_with_hidden_items();

        // 默认升序（`Sort::default`），文件夹永远在前。
        let hidden = Listing::read(&dir, false).expect("读得动");
        assert_eq!(
            names(&hidden),
            ["aaa.txt", "zzz.txt"],
            "默认不该看见 .config / .env"
        );
        // 开着的时候 `.config`（文件夹）排最前，其余按**小写**名升序——
        // `.` 的字节 0x2E 小于 `a` 的 0x61，所以 `.env` 紧跟着它。
        let shown = Listing::read(&dir, true).expect("读得动");
        assert_eq!(names(&shown), [".config", ".env", "aaa.txt", "zzz.txt"]);
    }

    /// **总数不变，只有可见数变。**
    ///
    /// 这条是那个设计决定的判据：状态栏那句「N / M 个项目」里的
    /// `M` 走 [`Listing::total_count`]。如果隐藏项在读盘时就被丢掉，
    /// 两个数字永远相等——用户按下开关看见 M 一点没变，
    /// 会以为开关坏了。
    #[test]
    fn switching_the_hidden_switch_moves_only_the_visible_count() {
        let (_keep, dir) = dir_with_hidden_items();
        let mut listing = Listing::read(&dir, false).expect("读得动");

        assert_eq!(listing.visible_count(), 2);
        assert_eq!(listing.total_count(), 4);

        assert!(listing.set_show_hidden(true), "第一次切要报告改了");
        assert_eq!(listing.visible_count(), 4);
        assert_eq!(listing.total_count(), 4, "磁盘上有几个就是几个");

        assert!(listing.set_show_hidden(false), "切回去也要报告改了");
        assert_eq!(listing.visible_count(), 2);
        // 再切一次同样的值不该报告任何改动——调用方靠这个返回值
        // 决定要不要重建行高缓存，报错了就是白白重建一次。
        assert!(!listing.set_show_hidden(false), "值没变就不该报告改了");
    }

    /// 切开关**不重读盘**：条目一直都在，只是没进可见列表。
    ///
    /// 判据是 `entry()`——它查的是 `entries`（磁盘上真实那份），
    /// 所以切完之后仍然能按路径查到那些项。顺手把 `position_of`
    /// 也钉一下：它查 `rank`，切完应该从 `None` 变成 `Some`。
    #[test]
    fn switching_does_not_lose_the_entries_themselves() {
        let (_keep, dir) = dir_with_hidden_items();
        let mut listing = Listing::read(&dir, false).expect("读得动");
        let env = dir.join(".env");

        assert!(listing.entry(&env).is_some(), "项一直都在");
        assert_eq!(listing.position_of(&env), None, "只是没显示");

        listing.set_show_hidden(true);

        assert!(listing.entry(&env).is_some());
        assert!(
            listing.position_of(&env).is_some(),
            "显示出来之后就该有位置了"
        );
    }

    /// 进了隐藏目录，里面的一切都显示出来。
    ///
    /// 不这么做的话，从命令行 `cd ~/.config` 进来会看到一个空目录——
    /// 而那个空目录只是"全被过滤掉了"。用户会以为 `.config` 是空的。
    /// 跟 Finder / 资源管理器一致。
    #[test]
    fn opening_a_hidden_directory_shows_what_is_inside_it() {
        let root = tempfile::tempdir().unwrap();
        // 藏在真名子目录底下：tempfile 在 macOS 上给的是 `.tmpXXXX`，
        // 那本身就是隐藏目录，测的就不是「隐藏目录里的东西可见」了。
        let hidden_dir = root.path().join("outer").join(".config");
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::write(hidden_dir.join(".gitignore"), "x").unwrap();
        fs::write(hidden_dir.join("hosts"), "x").unwrap();

        let listing = Listing::read(&hidden_dir, false).expect("读得动");
        assert!(
            listing.shows_hidden(),
            "正在看的目录自己是隐藏的，就得全显示"
        );
        assert_eq!(names(&listing), [".gitignore", "hosts"]);
    }

    /// 但**兄弟目录**不该跟着一起显示。
    ///
    /// 上一条容易被写成一个"凡是路径里有 `.` 就全显示"的实现，
    /// 那样从 `~` 进来就会冒出 `.ssh`、`.aws`——用户关着的开关就白关了。
    /// 判据只认**当前目录自己**的名字。
    #[test]
    fn a_normal_directory_still_hides_its_dot_entries() {
        let (_keep, dir) = dir_with_hidden_items();
        let listing = Listing::read(&dir, false).expect("读得动");
        assert!(!listing.shows_hidden());
        assert_eq!(names(&listing), ["aaa.txt", "zzz.txt"]);
    }

    /// 隐藏过滤与关键词过滤**同时作用**，而且是可交换的。
    ///
    /// 两个条件都在 [`Listing::refilter`] 的同一个 `.filter()` 链上，
    /// 这条防止"改成两趟，第二趟把第一趟的结果覆盖了"。
    #[test]
    fn the_hidden_filter_and_the_query_filter_both_apply() {
        let (_keep, dir) = dir_with_hidden_items();
        let mut listing = Listing::read(&dir, false).expect("读得动");

        // 关着隐藏 + 搜 "txt"：只有两个普通项，隐藏项照样不见。
        listing.set_query("txt");
        assert_eq!(names(&listing), ["aaa.txt", "zzz.txt"]);

        // 开着隐藏 + 同样搜 "txt"：`.env` 不匹配，`.config` 也不匹配。
        listing.set_show_hidden(true);
        assert_eq!(names(&listing), ["aaa.txt", "zzz.txt"]);

        // 搜 "e"：`.env` 现在能匹配了——它靠的是隐藏开关，不是查询。
        listing.set_query("e");
        assert_eq!(names(&listing), [".env"]);

        // 关掉隐藏，同一个查询立刻把它藏回去。
        listing.set_show_hidden(false);
        assert!(names(&listing).is_empty());
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

        assert!(Listing::read(&missing, false).is_err());

        let listing = Listing::empty(missing.clone());
        assert_eq!(listing.path(), missing.as_path());
        assert_eq!(listing.visible_count(), 0);
        assert_eq!(listing.position_of(&missing), None);
    }
}
