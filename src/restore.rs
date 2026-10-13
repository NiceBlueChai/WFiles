//! 崩溃恢复：记下崩溃前每一栏选中了什么、焦点在哪，下次启动原样接上。
//!
//! `session.rs` 已经记了"开着哪些标签、各自在看哪个目录"，所以崩了之后
//! 那几个目录是能回来的。**这一层补的是更上面一层**：用户当时正在处理
//! 那一批文件（已经选中的）和那个位置（键盘焦点），那才是下一步动作的起点。
//! 光恢复目录的话，在十万项的目录里再点一遍那一批，是一件很烦人的事。
//!
//! 三条取舍：
//!
//! - **单独存一份，不塞进 [`crate::session::Session`]。** 会话是"每次导航都
//!   写"的，恢复现场是"打开文件时才写"，两者的更新时机不一样；塞在一起
//!   会让导航顺手把恢复现场也写一遍，而它并不该那么频繁地变。
//! - **只记"哪些文件"和"焦点在哪"，不记滚动到第几行。** GPUI 的
//!   `ScrollHandle` 不暴露偏移量，想记也记不准；而"焦点在那个文件上"
//!   既能做到，体感也几乎一样——恢复完往那儿一滚，用户接着按方向键就是对的。
//! - **不记"上次崩了"这个事实。** 用户想看到的是自己打开的目录，
//!   而不是一条"上次异常退出"的提醒。真要有未完成的传输，
//!   进度条会自己说话（断点续传那套账本也已经在管了）。

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// 一个标签崩溃前的现场。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restore {
    /// 那一栏在看哪个目录。
    pub dir: PathBuf,
    /// 当时选中了哪些（按选择顺序，键盘操作按这个顺序来）。
    #[serde(default)]
    pub selected: Vec<PathBuf>,
    /// 键盘焦点在哪个文件上。
    #[serde(default)]
    pub cursor: Option<PathBuf>,
}

/// 全部标签的恢复现场，按目录索引。
///
/// 存成列表而不是 map：序列化的 map 键得是字符串，路径得转成字符串再转回来，
/// 而路径里有非 UTF-8 字节是常态（铁律 1）。列表直接存 [`PathBuf`]。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSet {
    #[serde(default)]
    pub items: Vec<Restore>,
}

impl RestoreSet {
    pub fn load() -> Self {
        Self::file_path()
            .and_then(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::file_path() else {
            return Err("找不到恢复文件位置".to_string());
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        // 跟会话同一套写法：先写临时文件再改名。
        // 崩溃恢复这件事本身就是在处理"崩溃写了一半"，
        // 这里再留一个写一半的恢复文件就太讽刺了。
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(json.as_bytes())
            .map_err(|error| error.to_string())?;
        fs::rename(&temp, path).map_err(|error| error.to_string())
    }

    /// 恢复文件放在哪儿。
    ///
    /// **测试构建不固定到某个目录**：跟会话文件走同一个状态目录
    /// （测试里由 `WFILES_SESSION_DIR` 指到临时目录），每个测试各自一份。
    ///
    /// 之前固定到 `test_root()` 那个共享位置，结果单测读到了 UI 测试
    /// 写进去的东西——单元测试之间本来不该互相看见对方的数据，
    /// 那种"并跑才挂"的失败最难查。
    fn file_path() -> Option<PathBuf> {
        // 与会话文件并排放：清状态目录时一起清，备份时一起备。
        crate::session::state_dir().map(|dir| dir.join("restore.json"))
    }

    /// 记下（或更新）这一栏的现场。
    pub fn note(&mut self, dir: PathBuf, selected: Vec<PathBuf>, cursor: Option<PathBuf>) {
        if selected.is_empty() && cursor.is_none() {
            // 什么都没选、也没落光标：这一栏就没有"现场"可言。
            // 留着一条空的，下次碰巧打开同一个目录时会以为该恢复点什么，
            // 然后什么也不发生——不如干脆删掉。
            self.items.retain(|item| item.dir != dir);
            return;
        }
        let item = Restore {
            dir,
            selected,
            cursor,
        };
        match self.items.iter_mut().find(|it| it.dir == item.dir) {
            Some(slot) => *slot = item,
            None => self.items.push(item),
        }
    }

    /// 那个目录的现场。
    pub fn at(&self, dir: &Path) -> Option<&Restore> {
        self.items.iter().find(|it| it.dir == dir)
    }

    /// 丢掉某一个目录的现场（标签被关掉时用）。
    ///
    /// 不靠"启动时统一清扫"来兜底：那份文件是每次选择变化都写的，
    /// 用户开着应用连关十个标签，那十个目录的现场就一直躺在盘上，
    /// 直到下次启动才被清掉。中间这段时间里，只要碰巧打开同一个目录，
    /// 就会冒出一批早就忘了的选中项。
    pub fn forget(&mut self, dir: &Path) {
        self.items.retain(|item| item.dir != dir);
    }

    /// 丢掉不再开着的那几栏，并剔除已经不存在的文件。
    ///
    /// `live_dirs` 是当前开着的那几个目录。
    ///
    /// 每一项剔除都对应一种"留着会出错"：
    ///
    /// - 目录不在 `live_dirs` 里 → 那是已经关掉的标签。留着的话，下次
    ///   碰巧打开同一个目录时会冒出一批莫名其妙的选中项。
    /// - 选中的文件已经不在了 → 在崩溃和重开之间被删掉/移走是常事。
    ///   `Cmd+C` 去拷一个不存在的路径，是明确的错误操作。
    pub fn sanitized(&mut self, live_dirs: &[PathBuf]) {
        self.items.retain(|item| live_dirs.contains(&item.dir));
        for item in &mut self.items {
            // 远程那一栏没法查磁盘（东西在服务器上），只能信记下来的样子。
            // 真失效了，恢复出来的那一项在操作时会自己报错，
            // 比在这里凭猜拦下一批正确的选择要好。
            if crate::remote::parse_location(&item.dir).is_some() {
                continue;
            }
            item.selected.retain(|path| path.exists());
            if let Some(cursor) = &item.cursor
                && !cursor.exists()
            {
                item.cursor = None;
            }
        }
        // 全空的也删掉：一份恢复文件里塞着几个空壳，只会越攒越多。
        self.items
            .retain(|item| !item.selected.is_empty() || item.cursor.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_set(items: Vec<Restore>) -> RestoreSet {
        RestoreSet { items }
    }

    fn item(dir: &str, selected: &[&str], cursor: Option<&str>) -> Restore {
        Restore {
            dir: PathBuf::from(dir),
            selected: selected.iter().map(PathBuf::from).collect(),
            cursor: cursor.map(PathBuf::from),
        }
    }

    #[test]
    fn a_saved_restore_comes_back_whole() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("restore.json");
        let original = restore_set(vec![item("/tmp", &["/tmp/a", "/tmp/b"], Some("/tmp/b"))]);
        original.save_to(&path).unwrap();

        assert_eq!(RestoreSet::load_from(&path).unwrap(), original);
    }

    #[test]
    fn a_missing_file_reads_as_nothing_to_restore() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(RestoreSet::load_from(&root.path().join("nope.json")), None);
        // `load()`/`save()` 走测试构建的固定位置（`test_root()`），
        // 而那里会被 UI 测试写进真的东西——所以这里只能验
        // "读不出来的当没有"，不能断言"读出来是空的"。
        let _ = RestoreSet::load();
    }

    #[test]
    fn a_corrupted_file_reads_as_nothing_rather_than_failing() {
        // 恢复文件坏了不该让应用起不来——那比"丢了选中项"严重得多。
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("restore.json");
        fs::write(&path, "{ 这不是 JSON").unwrap();
        assert_eq!(RestoreSet::load_from(&path), None);
    }

    #[test]
    fn an_old_file_without_the_new_field_still_loads() {
        // 格式演进：以后加字段必须带 `#[serde(default)]`，
        // 否则老用户升一次版本就丢了全部恢复现场。
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("restore.json");
        fs::write(&path, r#"{"items":[{"dir":"/tmp","selected":["/tmp/a"]}]}"#).unwrap();
        let loaded = RestoreSet::load_from(&path).expect("老文件也要能读");
        assert_eq!(loaded.items[0].selected.len(), 1);
        assert_eq!(loaded.items[0].cursor, None);
    }

    #[test]
    fn noting_the_same_pane_twice_replaces_rather_than_appends() {
        let mut set = RestoreSet::default();
        set.note(PathBuf::from("/tmp"), vec![PathBuf::from("/tmp/a")], None);
        set.note(
            PathBuf::from("/tmp"),
            vec![PathBuf::from("/tmp/b")],
            Some(PathBuf::from("/tmp/b")),
        );
        assert_eq!(set.items.len(), 1, "同一栏不该出现两条");
        assert_eq!(set.at(Path::new("/tmp")).unwrap().selected.len(), 1);
    }

    #[test]
    fn a_pane_with_nothing_selected_leaves_no_restore_behind() {
        // 开着目录但什么都没选：下次打开这个目录不该"恢复"出什么。
        let mut set = RestoreSet::default();
        set.note(PathBuf::from("/tmp"), vec![PathBuf::from("/tmp/a")], None);
        // 取消选择、也没有光标（用户 Ctrl+A 又取消、或 Esc 清了光标）。
        set.note(PathBuf::from("/tmp"), Vec::new(), None);
        assert!(
            set.items.is_empty(),
            "现场没了就该删掉，而不是留一条空的攒着"
        );
    }

    #[test]
    fn a_pane_with_only_a_cursor_is_still_worth_restoring() {
        let mut set = RestoreSet::default();
        set.note(
            PathBuf::from("/tmp"),
            Vec::new(),
            Some(PathBuf::from("/tmp/a")),
        );
        assert_eq!(
            set.at(Path::new("/tmp")).unwrap().cursor,
            Some(PathBuf::from("/tmp/a"))
        );
    }

    #[test]
    fn panes_that_are_closed_lose_their_restore() {
        let root = tempfile::tempdir().unwrap();
        let kept = root.path().join("kept");
        std::fs::create_dir_all(&kept).unwrap();
        let gone = root.path().join("gone");
        let a = kept.join("a.txt");
        std::fs::write(&a, "x").unwrap();

        let mut set = restore_set(vec![
            item(kept.to_str().unwrap(), &[a.to_str().unwrap()], None),
            item(gone.to_str().unwrap(), &["/不存在"], None),
        ]);
        set.sanitized(std::slice::from_ref(&kept));

        assert_eq!(set.items.len(), 1, "关掉的标签不该留着现场");
        assert!(set.at(&kept).is_some());
    }

    #[test]
    fn files_deleted_while_we_were_gone_are_dropped() {
        // 崩溃到重开之间，用户（或脚本、同步工具）删了其中一个文件。
        // 留着它等于下次 Cmd+C 去拷一个不存在的路径。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();
        let alive = dir.join("alive.txt");
        std::fs::write(&alive, "x").unwrap();
        let gone = dir.join("gone.txt");

        let mut set = restore_set(vec![item(
            dir.to_str().unwrap(),
            &[alive.to_str().unwrap(), gone.to_str().unwrap()],
            Some(gone.to_str().unwrap()),
        )]);
        set.sanitized(std::slice::from_ref(&dir));

        let left = set.at(&dir).unwrap();
        assert_eq!(
            left.selected,
            vec![alive.clone()],
            "没了的那个不该被恢复成选中"
        );
        assert_eq!(left.cursor, None, "光标指向没了的文件就该退回没有光标");
    }

    #[test]
    fn a_pane_whose_last_file_vanished_loses_the_whole_entry() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_path_buf();
        let mut set = restore_set(vec![item(dir.to_str().unwrap(), &["/早就不在了"], None)]);
        set.sanitized(&[dir]);
        assert!(set.items.is_empty(), "一条全空的现场就该删掉");
    }

    #[test]
    fn a_closed_tab_loses_its_restore_right_away() {
        // 跟 `sanitized` 的区别是时机：那个等下次启动才清，这个当场清。
        // 用户开着应用连关十个标签，那十个目录的现场不该一直躺在盘上。
        let mut set = restore_set(vec![
            item("/tmp/a", &["/tmp/a/x"], None),
            item("/tmp/b", &["/tmp/b/y"], None),
        ]);
        set.forget(Path::new("/tmp/a"));
        assert_eq!(set.items.len(), 1);
        assert!(set.at(Path::new("/tmp/a")).is_none());
        assert!(set.at(Path::new("/tmp/b")).is_some(), "别的目录不该被牵连");
    }

    #[test]
    fn forgetting_a_tab_that_never_had_a_restore_is_harmless() {
        let mut set = RestoreSet::default();
        set.forget(Path::new("/tmp/从没打开过"));
        assert!(set.items.is_empty());
    }

    #[test]
    fn a_remote_pane_is_trusted_because_it_cannot_be_checked() {
        // 远程那一栏的东西在服务器上，本地 `exists()` 一律为假。
        // 全当成"已删"的话，远程标签的选中项每次重启都会被清空——
        // 那等于这个功能对远程场景完全没用。
        let dir = PathBuf::from("dav://t1/资料");
        let mut set = restore_set(vec![item(
            dir.to_str().unwrap(),
            &["dav://t1/资料/方案.docx"],
            None,
        )]);
        set.sanitized(std::slice::from_ref(&dir));
        assert_eq!(
            set.at(&dir).unwrap().selected.len(),
            1,
            "远程的选中项不该被本地文件系统判成已删"
        );
    }
}
