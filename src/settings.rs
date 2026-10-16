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
    /// 用户改过的快捷键。没改过的命令不在这里，走出厂按键。
    #[serde(default)]
    pub keys: crate::keymap::KeyMap,
    /// 打在文件与文件夹上的标签和颜色。
    #[serde(default)]
    pub marks: crate::marks::Marks,
    /// 存下来的搜索：起了名字的查询，点一下就能再跑一遍。
    #[serde(default)]
    pub saved_searches: Vec<SavedSearch>,
    /// 保存下来的远程连接。**密码不在这里**——它在系统钥匙串里（见 `remote`）。
    #[serde(default)]
    pub remotes: Vec<crate::remote::Config>,
    /// 列表右侧那列元数据显示什么（存的是 `meta::Field` 的 id）；空就是不显示。
    ///
    /// 六种（Git 状态 / 图片尺寸 / 媒体时长 / EXIF / ID3 / PDF 页数）共用一列：
    /// 全铺开会挤掉文件名，而一次通常只关心其中一种。
    ///
    /// 存 id 字符串而不是枚举本身：手改配置写错一个字，最多这一列不出现，
    /// 不会让整份配置解析失败——那会把终端、标记、快捷键一起弄丢。
    #[serde(default)]
    pub meta_column: String,
    /// 搬运限速，单位是**字节每秒**；`0` = 不限速。
    ///
    /// 存裸数字而不是"1 MB/s"这样的字符串：解析一句人话要写一个
    /// 单位解析器，而写错了还得猜用户想说的是 1000 还是 1024。
    #[serde(default)]
    pub speed_limit: u64,
    /// 界面语言，存 `i18n::Locale::code()`（`zh-Hans` / `en`）。
    ///
    /// 存字符串而不是枚举，跟 `meta_column` 同一个理由：
    /// 手改配置写错一个字，最多退回"跟着系统走"，
    /// 不会让整份配置解析失败——那会把终端、标记、快捷键一起弄丢。
    /// 认不出来的串在 [`Settings::locale`] 里被当成"没设"。
    #[serde(default)]
    pub locale: String,
    /// 显示以 `.` 开头的隐藏项。
    ///
    /// **默认 `false`**，跟 Finder / 资源管理器 / GNOME Files 一致：
    /// 新手看到一堆 `.DS_Store`、`.git` 会以为文件管理器出问题了。
    ///
    /// 存成布尔而不是"三态"：正在看的那个目录自己是不是隐藏的，是另一回事
    /// —— 见 [`crate::model::Listing::read`]，它在读盘时单独处理
    /// （进了 `.config` 就把它里面的全显示出来），不写进这份设置。
    #[serde(default)]
    pub show_hidden: bool,
    /// 侧栏「收藏」段里的那些项，**新的排在最前面**。
    ///
    /// 存的是路径加"收藏那一刻它是不是目录"，不存显示名：
    /// 名字从 `file_name()` 推，跟标签栏同一套做法——用户把文件夹改了名，
    /// 收藏栏里跟着变，不需要在这里留一份会过期的副本。
    #[serde(default)]
    pub favorites: Vec<Favorite>,
}

/// 侧栏里收藏的一项。
///
/// **为什么存 `is_dir` 而不每次去问磁盘**：侧栏每一帧都要渲染这一段，
/// 十几次 `stat` 换来的是"列表刷新时才发现路径没了"。更重要的是
/// **收藏的目录被删掉之后**（用户明确选了"照旧显示，点进去才报"）：
/// `path.is_dir()` 此时会返回 false，于是点它会去叫系统打开一个
/// 不存在的目录，报出来的是"没有.application 能打开它"——那是误导。
/// 记下收藏那一刻的判断，点击时才知道用户当初收藏的是个目录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// 一个存下来的搜索。
///
/// 连"从哪个目录找起"一起存：同一个 `*.rs` 在 `~/work` 和 `~/photos`
/// 底下是两个完全不同的搜索，只存查询词会把它们混成一个。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub name: String,
    /// 查询原文（`Query::parse` 能再解析一遍的那种）。
    pub query: String,
    pub root: PathBuf,
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
        self.x + self.width > 160.
            && self.y + self.height > 120.
            && self.x < 20_000.
            && self.y < 20_000.
    }

    /// 错开一小步：副窗（`--new-window` 自己开的那一扇）开在主窗旁边。
    ///
    /// 不错开的话两扇窗**一模一样地叠着**——用户按了 `--new-window`，
    /// 屏幕上看起来"什么都没发生"。
    ///
    /// `visible` 是那块屏幕的可见范围 `(x, y, 宽, 高)`，也就是菜单栏、Dock、
    /// 任务栏之外那块地方；`None` = 拿不到屏幕信息，那就只错开、不钳制。
    /// 屏幕信息只有平台层知道，所以由调用方喂进来——这段算术于是是纯的，
    /// 能在没有屏幕的测试里钉死。
    pub fn cascaded(&self, visible: Option<(f32, f32, f32, f32)>) -> Self {
        /// 错开多少。32 px 是系统文件管理器和多数应用的惯常步长
        /// （差不多一个图标格），再小就看不出是两扇窗，再大就浪费地方。
        const STEP: f32 = 32.;

        let mut layout = Self {
            x: self.x + STEP,
            y: self.y + STEP,
            ..*self
        };
        if let Some((vx, vy, vw, vh)) = visible {
            // **装不下就贴回可见范围的左上**：主窗放大到占满屏幕时（最需要
            // 再开一扇窗的情形），往右下错 32 px 会把新窗的下边缘和右边缘
            // 推出屏幕。窗口比可见范围还大时 `max` 兜住负的上界——那种情况
            // 只能贴着左上，至少标题栏还能点。
            let max_x = (vx + vw - self.width).max(vx);
            let max_y = (vy + vh - self.height).max(vy);
            layout.x = layout.x.clamp(vx, max_x);
            layout.y = layout.y.clamp(vy, max_y);
        }
        layout
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
            keys: crate::keymap::KeyMap::default(),
            marks: crate::marks::Marks::default(),
            saved_searches: Vec::new(),
            remotes: Vec::new(),
            meta_column: String::new(),
            speed_limit: 0,
            show_hidden: false,
            favorites: Vec::new(),
            // 空串 = "跟着系统走"。不是直接写死中文：一份英文系统上刚装好
            // 就该是英文，让用户自己去设置里挑语言是把默认值做成了默认值。
            locale: String::new(),
        }
    }
}

impl Settings {
    /// 从磁盘读一份装进 `Global`。启动时调一次。
    ///
    /// 读不出来就用默认值——设置损坏不该挡住启动。
    pub fn install(cx: &mut impl gpui_kit::BorrowAppContext) {
        let mut settings = Self::load();
        // 删掉的文件不该在配置里一直占着位：启动时顺手清一次，
        // 真清掉了东西才写盘（平白写一次会把 mtime 也改了）。
        if settings.marks.drop_missing() {
            let _ = settings.save();
        }
        cx.set_global(settings);
    }

    /// 只读一眼，不克隆。
    ///
    /// 列表每一行都要问一次设置（这一格显示什么、有没有标色），
    /// 每行克隆一份设置——里面还带着全部标记——太贵了。
    pub fn peek<R>(cx: &gpui_kit::App, f: impl FnOnce(&Self) -> R) -> R {
        match cx.try_global::<Self>() {
            Some(settings) => f(settings),
            None => f(&Self::default()),
        }
    }

    /// 当前设置。没装 `Global` 时给默认值。
    pub fn read(cx: &gpui_kit::App) -> Self {
        Self::peek(cx, |settings| settings.clone())
    }

    /// 改设置：更新 `Global` 并立刻写盘。
    ///
    /// 写盘失败不回报给调用方：设置丢了最多是下次用默认值，
    /// 不该因为一个写不进的配置文件打断用户正在做的事。
    pub fn update<R>(
        cx: &mut impl gpui_kit::BorrowAppContext,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        cx.update_default_global::<Self, _>(|settings, _| {
            let result = f(settings);
            let _ = settings.save();
            result
        })
    }

    /// 当前生效的界面语言。
    ///
    /// 优先级：**命令行 > 设置 > 系统环境 > 中文**。
    /// 命令行排最前，因为它是"这一次启动我要什么"——脚本、录屏演示，
    /// 不该因为配置文件里存着别的语言就跑偏。
    ///
    /// 设置里那格存空串时**跟着系统环境走**（`LANG=zh_CN.UTF-8` → 简中），
    /// 环境也认不出来才落到中文。系统环境排在设置之后：用户在自己机器上
    /// 显式选过一次，比系统的全局默认更该算数。
    pub fn locale(&self) -> crate::i18n::Locale {
        crate::i18n::cli_locale()
            .or_else(|| crate::i18n::Locale::parse(&self.locale))
            .or_else(crate::i18n::Locale::from_env)
            .unwrap_or_default()
    }

    /// 记下界面语言。空串 = 跟着系统走。
    pub fn set_locale(&mut self, locale: crate::i18n::Locale) {
        self.locale = locale.code().to_string();
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
        if let Some(position) = self
            .folder_views
            .iter()
            .position(|entry| entry.path == path)
        {
            self.folder_views.remove(position);
        } else if self.folder_views.len() >= MAX_FOLDER_VIEWS {
            self.folder_views.pop();
        }
        self.folder_views.insert(0, FolderView { path, sort, view });
    }

    /// 这一项在不在收藏里。
    pub fn is_favorite(&self, path: &Path) -> bool {
        self.favorites.iter().any(|it| it.path == path)
    }

    /// 收藏 / 取消收藏这一个路径，返回切换之后它**在不在**收藏里。
    ///
    /// 收藏就**插到最前面**（用户 2026-10-14 拍板"加入顺序，新的在最上"）：
    /// 新收藏的那个最可能是他刚要用的，而收藏多了自然等于常用位置——
    /// 这也是「最近文件」不单独做的理由。
    ///
    /// 已在收藏里时取消它**不重排**（不移到末尾）：取消不是一次"添加"，
    /// 把它的位置换掉会让用户以为自己弄丢了顺序。
    pub fn toggle_favorite(&mut self, path: PathBuf, is_dir: bool) -> bool {
        match self.favorites.iter().position(|it| it.path == path) {
            Some(position) => {
                self.favorites.remove(position);
                false
            }
            None => {
                self.favorites.insert(0, Favorite { path, is_dir });
                true
            }
        }
    }

    /// 按名字找一条存下来的搜索。
    pub fn saved_search(&self, name: &str) -> Option<&SavedSearch> {
        self.saved_searches.iter().find(|it| it.name == name)
    }
    /// 存一条搜索。同名就覆盖——用户改了查询又存一次，多半是想换掉旧的。
    ///
    /// 名字是空的就不存：一条没有名字的保存搜索在界面上没法点。
    pub fn save_search(&mut self, name: String, query: String, root: PathBuf) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let saved = SavedSearch { name, query, root };
        match self
            .saved_searches
            .iter()
            .position(|it| it.name == saved.name)
        {
            Some(position) => self.saved_searches[position] = saved,
            None => self.saved_searches.push(saved),
        }
    }

    /// 忘掉一条存下来的搜索。
    pub fn forget_search(&mut self, name: &str) {
        self.saved_searches.retain(|it| it.name != name);
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

    pub fn save(&self) -> Result<(), crate::i18n::Msg> {
        let Some(path) = Self::file_path() else {
            return Err(crate::i18n::Message::msg("找不到设置文件位置"));
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), crate::i18n::Msg> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(crate::i18n::Message::from)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|error| {
            crate::i18n::Message::caused("序列化失败：{error}", error.to_string())
        })?;
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).map_err(crate::i18n::Message::from)?;
        file.write_all(json.as_bytes())
            .map_err(crate::i18n::Message::from)?;
        fs::rename(&temp, path).map_err(crate::i18n::Message::from)
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
    fn a_saved_search_comes_back_with_its_root() {
        let mut stored = Settings::default();
        let root = PathBuf::from("/Users/me/Projects");
        stored.save_search("找图".to_string(), "*.png".to_string(), root.clone());

        let saved = stored.saved_search("找图").expect("该存下来了");
        assert_eq!(saved.query, "*.png");
        assert_eq!(saved.root, root);

        // 整份设置走一遍磁盘也不该丢。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        stored.save_to(&path).unwrap();
        let back = Settings::load_from(&path).expect("读回来");
        assert_eq!(back.saved_searches, stored.saved_searches);
    }

    #[test]
    fn saving_the_same_name_replaces_the_old_one() {
        let mut stored = Settings::default();
        let root = PathBuf::from("/tmp");
        stored.save_search("找图".to_string(), "*.png".to_string(), root.clone());
        stored.save_search("找图".to_string(), "size:>1M".to_string(), root.clone());

        assert_eq!(stored.saved_searches.len(), 1, "同名不该存两条");
        assert_eq!(stored.saved_search("找图").expect("还在").query, "size:>1M");
    }

    #[test]
    fn a_nameless_search_is_not_saved() {
        let mut stored = Settings::default();
        stored.save_search("   ".to_string(), "*.rs".to_string(), PathBuf::from("/tmp"));
        assert!(stored.saved_searches.is_empty(), "没名字就没法在界面上点它");
    }

    #[test]
    fn a_saved_search_can_be_forgotten() {
        let mut stored = Settings::default();
        stored.save_search(
            "找图".to_string(),
            "*.png".to_string(),
            PathBuf::from("/tmp"),
        );
        stored.save_search(
            "找码".to_string(),
            "*.rs".to_string(),
            PathBuf::from("/tmp"),
        );
        stored.forget_search("找图");
        assert_eq!(stored.saved_searches.len(), 1);
        assert_eq!(stored.saved_search("找图"), None);
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
        stored.remember_view(
            projects.clone(),
            sort_by(SortKey::Name),
            ViewMode::default(),
        );

        assert_eq!(
            sort_for(&stored, &downloads),
            Some(sort_by(SortKey::Modified))
        );
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
        assert_eq!(
            sort_for(&loaded, &PathBuf::from("/a")),
            Some(Sort::default())
        );
        assert_eq!(
            loaded.view_for(&PathBuf::from("/a")).map(|view| view.view),
            Some(ViewMode::List)
        );
    }

    #[test]
    fn a_favorite_goes_to_the_front_and_comes_back_out() {
        let mut stored = Settings::default();
        assert!(!stored.is_favorite(&PathBuf::from("/a")));

        assert!(
            stored.toggle_favorite(PathBuf::from("/a"), true),
            "第一次是收藏"
        );
        assert!(stored.is_favorite(&PathBuf::from("/a")));
        assert_eq!(stored.favorites.len(), 1);
        // 收藏那一刻它是不是目录，得原样记下来。
        assert!(stored.favorites[0].is_dir);

        // 新的排在最前面（用户拍板"加入顺序，新的在最上"）。
        stored.toggle_favorite(PathBuf::from("/b"), false);
        assert_eq!(stored.favorites[0].path, PathBuf::from("/b"));
        assert_eq!(stored.favorites[1].path, PathBuf::from("/a"));
        assert!(!stored.favorites[0].is_dir, "文件也收，得记住它是文件");

        assert!(
            !stored.toggle_favorite(PathBuf::from("/b"), false),
            "再按是取消"
        );
        assert!(!stored.is_favorite(&PathBuf::from("/b")));
        assert_eq!(
            stored.favorites[0].path,
            PathBuf::from("/a"),
            "取消不该把剩下的重排——那会让用户以为顺序被弄丢了"
        );
        assert_eq!(stored.favorites.len(), 1);
    }

    #[test]
    fn favorites_survive_the_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let mut stored = settings();
        stored.toggle_favorite(PathBuf::from("/Users/me/项目"), true);
        stored.toggle_favorite(PathBuf::from("/Users/me/notes.md"), false);

        stored.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path).expect("读回来");

        assert_eq!(loaded.favorites, stored.favorites);
        assert_eq!(
            loaded.favorites[0].path,
            PathBuf::from("/Users/me/notes.md")
        );
        assert!(loaded.is_favorite(&PathBuf::from("/Users/me/项目")));
    }

    #[test]
    fn an_old_settings_file_without_favorites_still_loads() {
        // 收藏是新加的：升级上来的人那份配置里没有这个字段，
        // 读出来必须是空的，不能报"配置损坏"——那会把用户的终端、
        // 标记、快捷键一起弄丢。
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        std::fs::write(&path, r#"{"terminal":"auto"}"#).unwrap();

        let loaded = Settings::load_from(&path).expect("老配置也要读得进");
        assert!(loaded.favorites.is_empty());
        assert!(!loaded.is_favorite(&PathBuf::from("/a")));
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
    fn a_secondary_window_steps_down_and_right() {
        let base = Layout {
            x: 100.,
            y: 50.,
            width: 800.,
            height: 600.,
        };
        let stepped = base.cascaded(Some((0., 0., 1920., 1080.)));
        assert_eq!(stepped.x, 132.);
        assert_eq!(stepped.y, 82.);
        assert_eq!(
            (stepped.width, stepped.height),
            (800., 600.),
            "错开的只有位置——副窗跟主窗一样大"
        );
    }

    #[test]
    fn a_secondary_window_never_steps_off_screen() {
        // 放大到占满整块屏的主窗（最需要再开一扇窗的情形）：往右下错 32 px
        // 会把新窗的下边缘和右边缘推出屏幕。装不下就贴回可见范围的左上。
        let full = Layout {
            x: 0.,
            y: 25.,
            width: 1920.,
            height: 1055.,
        };
        let stepped = full.cascaded(Some((0., 25., 1920., 1055.)));
        assert_eq!((stepped.x, stepped.y), (0., 25.));
    }

    #[test]
    fn a_secondary_window_larger_than_the_screen_still_has_a_visible_corner() {
        // 比可见范围还大的主窗（外接屏拔了、手改了配置都有这种）。
        // 不能钳出负坐标——那样标题栏就点不到了。
        let huge = Layout {
            x: 900.,
            y: 700.,
            width: 2400.,
            height: 1400.,
        };
        let stepped = huge.cascaded(Some((0., 0., 1920., 1080.)));
        assert_eq!((stepped.x, stepped.y), (0., 0.));
    }

    #[test]
    fn a_secondary_window_on_the_second_screen_steps_within_it() {
        // 外接屏在主屏左边时坐标是负的：钳制要按那块屏的可见范围来，
        // 不是一律往 0 上贴（那样会把窗挪回主屏，用户找不着）。
        let base = Layout {
            x: -1800.,
            y: 100.,
            width: 800.,
            height: 600.,
        };
        let stepped = base.cascaded(Some((-1920., 0., 1920., 1080.)));
        assert_eq!(stepped.x, -1768.);
    }

    #[test]
    fn without_screen_information_a_secondary_window_still_steps() {
        // 拿不到屏幕信息（平台层没给）时只错开、不钳制：
        // 至少不会两扇窗严丝合缝地叠着。
        let base = Layout {
            x: -100.,
            y: 40.,
            width: 800.,
            height: 600.,
        };
        assert_eq!(base.cascaded(None).x, -68.);
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
