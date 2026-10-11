//! 文件条目与目录内容模型。
//!
//! 这一层不依赖任何 UI 类型：它只负责读目录、排序、过滤，以及记住哪些路径被选中。

use std::{
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

/// 一个目录的内容，连同它的过滤状态。
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
    query: String,
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
            query: String::new(),
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
            query: String::new(),
        }
    }

    fn rebuild(&mut self) {
        let entries = &self.entries;
        let mut order: Vec<usize> = (0..entries.len()).collect();
        // 文件夹永远排在前面，名称比较用预计算的小写副本，避免每帧重新分配。
        order.sort_by(|&left, &right| {
            entries[right]
                .is_dir
                .cmp(&entries[left].is_dir)
                .then_with(|| entries[left].sort_name.cmp(&entries[right].sort_name))
        });

        let query = self.query.trim().to_lowercase();
        self.order = if query.is_empty() {
            order
        } else {
            order
                .into_iter()
                .filter(|&ix| entries[ix].sort_name.contains(&query))
                .collect()
        };
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
}

/// 当前选中的路径集合。
///
/// 用路径而不是下标做键：刷新之后选中状态仍然指向同一批文件。
#[derive(Debug, Default)]
pub struct Selection {
    paths: BTreeSet<PathBuf>,
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    pub fn len(&self) -> usize {
        self.paths.len()
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
        self.anchor = Some(path.clone());
        self.paths.insert(path);
    }

    /// 目录刷新后丢掉已经不存在的路径。
    pub fn retain_existing(&mut self, listing: &Listing) {
        let before = self.paths.len();
        self.paths.retain(|path| listing.entry(path).is_some());

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
    fn filter_narrows_the_visible_order() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("alpha.txt"), "x").unwrap();
        fs::write(root.path().join("beta.log"), "x").unwrap();

        let mut listing = listing_of(root.path());
        assert_eq!(listing.visible_count(), 2);

        listing.set_query("beta");
        assert_eq!(names(&listing), ["beta.log"]);
        assert_eq!(listing.total_count(), 2);

        listing.set_query("");
        assert_eq!(listing.visible_count(), 2);
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
    }
}
