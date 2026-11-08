//! 标签与颜色：给文件或文件夹做个记号。
//!
//! **按路径记**，不跟 inode、不管文件系统事件。代价是文件被搬走或删掉之后
//! 记号会悬空（再建一个同名文件会把旧记号捡回来）；换来的是不用监听整个磁盘、
//! 也不用为了一个记号去读文件系统的元数据。对"给我最近在看的几个文件标个色"
//! 这种用法，这个取舍是划算的。
//!
//! 这一层不认识界面：它只管"哪个路径有什么记号"以及持久化格式。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 七种标记色：`(写到配置里的名字, 界面上显示的名字)`。
pub const COLORS: [(&str, &str); 7] = [
    ("red", "红"),
    ("orange", "橙"),
    ("yellow", "黄"),
    ("green", "绿"),
    ("blue", "蓝"),
    ("purple", "紫"),
    ("gray", "灰"),
];

/// 界面画色块用的 RGB。放在这一层只是因为它们得和 [`COLORS`] 一一对应，
/// 免得加一种颜色要改两个文件——这里没有任何 GPUI 依赖。
pub fn color_hex(name: &str) -> u32 {
    match name {
        "orange" => 0xf97316,
        "yellow" => 0xeab308,
        "green" => 0x22c55e,
        "blue" => 0x3b82f6,
        "purple" => 0xa855f7,
        "gray" => 0x94a3b8,
        _ => 0xef4444,
    }
}

/// 一个路径上的记号。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// 颜色名（[`COLORS`] 里的第一个元素）；`None` 是没标色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Mark {
    pub fn is_empty(&self) -> bool {
        self.color.is_none() && self.tags.is_empty()
    }
}

/// 全部记号：路径 → 记号。
///
/// 用 `BTreeMap<String, _>` 而不是 `BTreeMap<PathBuf, _>`：
/// JSON 的对象键只能是字符串，`PathBuf` 作键序列化不过去。
/// 写出来的文件按路径排序，手打开也能找得到。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks(pub BTreeMap<String, Mark>);

impl Marks {
    pub fn get(&self, path: &Path) -> Option<&Mark> {
        self.0.get(&key(path))
    }

    pub fn color_of(&self, path: &Path) -> Option<&str> {
        self.get(path)?.color.as_deref()
    }

    pub fn tags_of(&self, path: &Path) -> &[String] {
        match self.get(path) {
            Some(mark) => &mark.tags,
            None => &[],
        }
    }

    /// 标个色；`None` 是把颜色去掉（标签不受影响）。
    pub fn set_color(&mut self, path: &Path, color: Option<String>) {
        let key = key(path);
        let entry = self.0.entry(key).or_default();
        entry.color = color;
        if entry.is_empty() {
            self.0.remove(&path.to_string_lossy().into_owned());
        }
    }

    /// 换掉全部标签。
    pub fn set_tags(&mut self, path: &Path, tags: Vec<String>) {
        let key = key(path);
        let entry = self.0.entry(key).or_default();
        entry.tags = tags;
        if entry.is_empty() {
            self.0.remove(&path.to_string_lossy().into_owned());
        }
    }

    /// 连颜色带标签一起去掉。
    pub fn clear(&mut self, path: &Path) {
        self.0.remove(&key(path));
    }

    /// 用过的所有标签（去重、排好序）：界面上拿来提示"你以前用过什么"。
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .0
            .values()
            .flat_map(|mark| mark.tags.iter().cloned())
            .collect();
        tags.sort_unstable();
        tags.dedup();
        tags
    }

    /// 丢掉已经不存在的路径：删掉的文件不该在配置文件里一直占位。
    ///
    /// 只在真的有新东西可丢时才返回 `true`，调用方据此决定要不要落盘。
    pub fn drop_missing(&mut self) -> bool {
        let before = self.0.len();
        self.0.retain(|path, _| Path::new(path).exists());
        self.0.len() != before
    }
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 这是不是一种认识的颜色。查询语法里 `color:红` 要能报"没有这个颜色"。
pub fn is_color(name: &str) -> bool {
    COLORS.iter().any(|(id, _)| *id == name)
}

/// 界面上这个颜色叫什么。`red` → `红`。
pub fn color_label(name: &str) -> &str {
    COLORS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, label)| *label)
        .unwrap_or(name)
}

/// 从一行输入里拆出标签：逗号、顿号、空格都算分隔，空白项丢掉。
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = input
        .split([',', '，', '、', ' ', '\t'])
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort_unstable();
    tags.dedup();
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path(name: &str) -> PathBuf {
        PathBuf::from("/tmp").join(name)
    }

    #[test]
    fn a_mark_round_trips_through_json() {
        let mut marks = Marks::default();
        marks.set_color(&path("a.txt"), Some("red".to_string()));
        marks.set_tags(&path("a.txt"), vec!["工作".to_string()]);

        let json = serde_json::to_string(&marks).expect("序列化");
        let back: Marks = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, marks);
        assert_eq!(back.color_of(&path("a.txt")), Some("red"));
        assert_eq!(back.tags_of(&path("a.txt")), ["工作"]);
    }

    #[test]
    fn clearing_the_last_bit_removes_the_entry() {
        let mut marks = Marks::default();
        marks.set_color(&path("a.txt"), Some("red".to_string()));
        marks.set_color(&path("a.txt"), None);
        assert!(marks.0.is_empty(), "颜色去掉了就不该留条目");

        marks.set_tags(&path("b.txt"), vec!["x".to_string()]);
        marks.set_tags(&path("b.txt"), Vec::new());
        assert!(marks.0.is_empty(), "标签清空了就不该留条目");
    }

    #[test]
    fn a_color_and_tags_live_side_by_side() {
        let mut marks = Marks::default();
        marks.set_tags(&path("a.txt"), vec!["工作".to_string()]);
        marks.set_color(&path("a.txt"), Some("blue".to_string()));
        assert_eq!(marks.tags_of(&path("a.txt")), ["工作"]);
        assert_eq!(marks.color_of(&path("a.txt")), Some("blue"));
    }

    #[test]
    fn clear_takes_everything_away() {
        let mut marks = Marks::default();
        marks.set_color(&path("a.txt"), Some("red".to_string()));
        marks.set_tags(&path("a.txt"), vec!["工作".to_string()]);
        marks.clear(&path("a.txt"));
        assert!(marks.0.is_empty());
    }

    #[test]
    fn used_tags_are_listed_once_and_sorted() {
        let mut marks = Marks::default();
        marks.set_tags(&path("a.txt"), vec!["工作".to_string(), "重要".to_string()]);
        marks.set_tags(&path("b.txt"), vec!["工作".to_string()]);
        assert_eq!(marks.all_tags(), ["工作", "重要"]);
    }

    #[test]
    fn tags_are_split_on_commas_and_spaces() {
        assert_eq!(parse_tags("工作, 重要"), ["工作", "重要"]);
        assert_eq!(parse_tags("工作 重要、待办"), ["工作", "待办", "重要"]);
        assert_eq!(parse_tags("   "), [] as [String; 0]);
        assert_eq!(parse_tags("工作 工作"), ["工作"], "重复的只留一个");
    }

    #[test]
    fn only_known_colors_are_accepted() {
        assert!(is_color("red"));
        assert!(is_color("purple"));
        assert!(!is_color("pink"));
        assert_eq!(color_label("green"), "绿");
        assert_eq!(color_label("pink"), "pink", "不认识的就原样显示");
    }

    #[test]
    fn missing_paths_can_be_dropped() {
        let mut marks = Marks::default();
        let dir = tempfile::tempdir().expect("临时目录");
        let alive = dir.path().join("here.txt");
        std::fs::write(&alive, "x").expect("写文件");
        marks.set_color(&alive, Some("red".to_string()));
        marks.set_color(&path("gone.txt"), Some("blue".to_string()));

        assert!(marks.drop_missing(), "该报告真的丢掉了东西");
        assert_eq!(marks.0.len(), 1);
        assert!(marks.color_of(&alive) == Some("red"));
        assert!(!marks.drop_missing(), "第二次没东西可丢");
    }

    #[test]
    fn colors_and_hexes_line_up() {
        for (name, _) in COLORS {
            assert!(
                color_hex(name) != color_hex("不存在的颜色") || name == "red",
                "{name} 该有自己的色值"
            );
        }
    }
}
