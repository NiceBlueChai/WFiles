//! 全局设置：跨重启记住的偏好。
//!
//! 存成一个 GPUI `Global`，这样任何实体（窗格、应用）都能读到当前值，
//! 不用把设置顺着构造参数一层层往下传。改设置即落盘：
//! [`Settings::update`] 是唯一写入口，不会出现“界面上改了、重启又回去”。

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::model::{Sort, ViewMode};
use crate::terminal;

/// 最多记住多少个目录的视图。
///
/// 上限不是怕内存（一项才几十字节），是怕配置文件无限膨胀：
/// 一个天天在几万个目录里乱逛的人不该背着一个几 MB 的 settings.json。
const MAX_FOLDER_VIEWS: usize = 500;

/// 需要跨重启记住的偏好。
// 只 PartialEq 不 Eq：Layout 里是像素坐标（f32），浮点没有 Eq。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// 打开终端用哪个程序；`terminal::AUTO` 表示跟着系统走。
    #[serde(default = "default_terminal")]
    pub terminal: String,
    /// 每个目录记住的视图。最近用过的排在最前面。
    #[serde(default)]
    pub folder_views: Vec<FolderView>,
    /// 上次退出时的窗口位置与尺寸。
    #[serde(default)]
    pub layout: Option<Layout>,
}

/// 一个目录记住的视图设置。
///
/// 排序之外现在还有视图模式（列表/网格/分列）——等有了可拖动列宽，
/// 往这里加字段就行，读取侧不必改（旧配置缺字段会走 `Default`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderView {
    pub path: PathBuf,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub view: ViewMode,
}

/// 工作区布局：窗口左上角与尺寸，单位像素。
///
/// 存成四个数字而不是 `WindowBounds`：那个枚举带最大化/全屏状态，
/// 序列化它会把 UI 框架的类型钉死在磁盘格式上。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Layout {
    /// 最小可用尺寸：再小就装不下工具栏加一行列表了。
    const MIN_WIDTH: f32 = 480.;
    const MIN_HEIGHT: f32 = 320.;

    /// 这份布局还能用吗。
    ///
    /// 用户可能换了显示器、拔了外接屏、或者手改了配置文件：
    /// 一份把窗口放到屏幕外的布局会让应用"打不开"，
    /// 而这正是最难自己排查的故障。宁可退回默认尺寸。
    pub fn usable(&self) -> bool {
        let finite = [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite());
        if !finite {
            return false;
        }
        if self.width < Self::MIN_WIDTH || self.height < Self::MIN_HEIGHT {
            return false;
        }
        // 至少得有一部分落在"第一块屏幕（原点在左上）"里。
        // 外接屏在左边时 x 可以是负的，所以只要求右侧/下侧还看得见。
        self.x + self.width > 160. && self.y + self.height > 120. && self.x < 20_000. && self.y < 20_000.
    }
}

fn default_terminal() -> String {
    terminal::AUTO.to_string()
}

// GPUI 的 Global 是个空标记 trait：实现它才允许把 Settings 存进全局槽位。
impl gpui_kit::Global for Settings {}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal: default_terminal(),
            folder_views: Vec::new(),
            layout: None,
        }
    }
}

impl Settings {
    /// 从磁盘读一份装进 `Global`。启动时调一次。
    ///
    /// 读不出来就用默认值——设置损坏不该挡住启动。
    pub fn install(cx: &mut impl gpui_kit::BorrowAppContext) {
        let settings = Self::load();
        cx.set_global(settings);
    }

    /// 当前设置。没装 `Global` 时给默认值。
    pub fn read(cx: &gpui_kit::App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// 改设置：更新 `Global` 并立刻写盘。
    ///
    /// 写盘失败不回报给调用方：设置丢了最多是下次用默认值，
    /// 不该因为一个写不进的配置文件打断用户正在做的事。
    pub fn update<R>(cx: &mut impl gpui_kit::BorrowAppContext, f: impl FnOnce(&mut Self) -> R) -> R {
        cx.update_default_global::<Self, _>(|settings, _| {
            let result = f(settings);
            let _ = settings.save();
            result
        })
    }

    /// 这个目录上次用的排序与视图；没记住过就是 `None`，由调用方决定默认值。
    pub fn view_for(&self, path: &Path) -> Option<FolderView> {
        self.folder_views
            .iter()
            .find(|view| view.path == path)
            .cloned()
    }

    /// 记住这个目录的排序与视图。
    ///
    /// 同一个目录再来就更新并挪到最前面，超过上限丢最久没用的那个——
    /// 这是"最近用过的目录"，不是"去过的所有目录"。
    pub fn remember_view(&mut self, path: PathBuf, sort: Sort, view: ViewMode) {
        if let Some(position) = self.folder_views.iter().position(|entry| entry.path == path) {
            self.folder_views.remove(position);
        } else if self.folder_views.len() >= MAX_FOLDER_VIEWS {
            self.folder_views.pop();
        }
        self.folder_views.insert(0, FolderView { path, sort, view });
    }

    pub fn file_path() -> Option<PathBuf> {
        crate::session::state_dir().map(|dir| dir.join("settings.json"))
    }

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
            return Err("找不到设置文件位置".to_string());
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(json.as_bytes())
            .map_err(|error| error.to_string())?;
        fs::rename(&temp, path).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SortKey;

    fn settings() -> Settings {
        Settings {
            terminal: "iterm".to_string(),
            ..Settings::default()
        }
    }

    fn sort_by(key: SortKey) -> Sort {
        Sort {
            key,
            descending: true,
        }
    }

    /// 这个目录记住的排序；视图模式单独断言。
    fn sort_for(stored: &Settings, path: &Path) -> Option<Sort> {
        stored.view_for(path).map(|view| view.sort)
    }

    #[test]
    fn roundtrip_preserves_the_terminal_choice() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("settings.json");

        let settings = settings();
        settings.save_to(&path).unwrap();

        assert_eq!(Settings::load_from(&path), Some(settings));
    }

    #[test]
    fn a_remembered_sort_comes_back_for_that_folder() {
        let mut stored = Settings::default();
        let downloads = PathBuf::from("/Users/me/Downloads");
        let projects = PathBuf::from("/Users/me/Projects");

        stored.remember_view(
            downloads.clone(),
            sort_by(SortKey::Modified),
            ViewMode::Grid,
        );
        stored.remember_view(projects.clone(), sort_by(SortKey::Name), ViewMode::default());

        assert_eq!(sort_for(&stored, &downloads), Some(sort_by(SortKey::Modified)));
        assert_eq!(sort_for(&stored, &projects), Some(sort_by(SortKey::Name)));
        assert_eq!(
            stored.view_for(&PathBuf::from("/Users/me/Music")),
            None,
            "没去过的地方就该说没记住"
        );
        // 视图模式和排序是一起记住的：换视图不该丢排序，换排序也不该丢视图。
        assert_eq!(
            stored.view_for(&downloads).map(|view| view.view),
            Some(ViewMode::Grid)
        );
    }

    #[test]
    fn remembering_twice_updates_instead_of_duplicating() {
        let mut stored = Settings::default();
        let path = PathBuf::from("/a");
        stored.remember_view(path.clone(), sort_by(SortKey::Name), ViewMode::default());
        stored.remember_view(path.clone(), sort_by(SortKey::Size), ViewMode::Grid);

        assert_eq!(stored.folder_views.len(), 1);
        assert_eq!(sort_for(&stored, &path), Some(sort_by(SortKey::Size)));
        assert_eq!(
            stored.view_for(&path).map(|view| view.view),
            Some(ViewMode::Grid)
        );
    }

    #[test]
    fn the_most_recent_folder_stays_at_the_front() {
        let mut stored = Settings::default();
        stored.remember_view(PathBuf::from("/a"), Sort::default(), ViewMode::default());
        stored.remember_view(PathBuf::from("/b"), Sort::default(), ViewMode::default());
        // 再回一次 /a：它该重新排到最前面。
        stored.remember_view(PathBuf::from("/a"), Sort::default(), ViewMode::default());

        assert_eq!(stored.folder_views[0].path, PathBuf::from("/a"));
        assert_eq!(stored.folder_views.len(), 2);
    }

    #[test]
    fn the_view_list_stops_growing() {
        let mut stored = Settings::default();
        for index in 0..MAX_FOLDER_VIEWS + 50 {
            stored.remember_view(
                PathBuf::from(format!("/d{index}")),
                Sort::default(),
                ViewMode::default(),
            );
        }

        assert_eq!(stored.folder_views.len(), MAX_FOLDER_VIEWS);
        assert_eq!(
            stored.folder_views[0].path,
            PathBuf::from(format!("/d{}", MAX_FOLDER_VIEWS + 49)),
            "最后去的那个要在最前面"
        );
        assert!(
            stored.view_for(&PathBuf::from("/d0")).is_none(),
            "最早去过的应该被挤掉"
        );
    }

    #[test]
    fn folder_views_survive_the_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let mut stored = settings();
        stored.remember_view(
            PathBuf::from("/Users/me/Downloads"),
            sort_by(SortKey::Modified),
            ViewMode::Grid,
        );
        stored.layout = Some(Layout {
            x: 10.,
            y: 20.,
            width: 1180.,
            height: 760.,
        });

        stored.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path).expect("读回来");

        assert_eq!(
            sort_for(&loaded, &PathBuf::from("/Users/me/Downloads")),
            Some(sort_by(SortKey::Modified))
        );
        assert_eq!(
            loaded
                .view_for(&PathBuf::from("/Users/me/Downloads"))
                .map(|view| view.view),
            Some(ViewMode::Grid)
        );
        assert_eq!(loaded.layout, stored.layout);
    }

    #[test]
    fn an_old_settings_file_without_a_view_mode_still_loads() {
        // 视图模式是新加的：升级上来的人那份配置里没有这个字段，
        // 读出来必须退回列表，不能直接报"配置损坏"。
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"terminal":"auto","folder_views":[{"path":"/a","sort":{"key":"Name","descending":false}}]}"#,
        )
        .unwrap();

        let loaded = Settings::load_from(&path).expect("老配置也要读得进");
        assert_eq!(sort_for(&loaded, &PathBuf::from("/a")), Some(Sort::default()));
        assert_eq!(
            loaded.view_for(&PathBuf::from("/a")).map(|view| view.view),
            Some(ViewMode::List)
        );
    }

    #[test]
    fn a_layout_that_would_hide_the_window_is_rejected() {
        let sane = Layout {
            x: 100.,
            y: 100.,
            width: 1180.,
            height: 760.,
        };
        assert!(sane.usable());

        let tiny = Layout {
            width: 10.,
            height: 10.,
            ..sane
        };
        assert!(!tiny.usable(), "10×10 的窗口等于没开");

        let offscreen = Layout {
            x: -40_000.,
            ..sane
        };
        assert!(!offscreen.usable(), "整个跑到屏幕外的窗口等于没开");

        // 外接屏在主屏左边时 x 是负的，只要还有一部分看得见就该接受。
        let partly_offscreen = Layout { x: -600., ..sane };
        assert!(partly_offscreen.usable());

        let nonsense = Layout {
            x: f32::NAN,
            ..sane
        };
        assert!(!nonsense.usable(), "NaN 不能算能用");
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(Settings::default().terminal, terminal::AUTO);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 旧版本写的设置没有新字段，读出来不能变成空字符串。
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        fs::write(&path, "{}").unwrap();

        assert_eq!(Settings::load_from(&path), Some(Settings::default()));
    }

    #[test]
    fn corrupt_file_yields_default() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        fs::write(&path, "{ 这不是 json").unwrap();

        assert_eq!(Settings::load_from(&path), None);
    }

    #[test]
    fn missing_file_yields_none() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load_from(&root.path().join("nope.json")), None);
    }
}
