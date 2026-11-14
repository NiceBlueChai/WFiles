//! 侧栏「收藏」：哪几个路径被收着了，以及怎么加减。
//!
//! # 为什么归 core
//!
//! 收藏是**用户数据**，不是某个前端的私有状态：GUI 版与 NSH 客户端看的是同一批
//! 收藏。放在界面层就会变成"各存一份、各改各的"，用户换一个前端发现收藏空了，
//! 还说不出是谁弄丢的。所以位置、结构、加减规则都在这里，两端只负责画。
//!
//! # 为什么读写整个 `settings.json` 而不是另开一个文件
//!
//! 老版本把收藏存在 `settings.json` 的 `favorites` 键里。另开一个文件等于让
//! 已有用户的收藏"凭空消失"，而且两边早晚会不一致。
//!
//! 这里用 [`serde_json::Value`] **只动 `favorites` 这一个键，其余字段原样写回**：
//! 界面版本升级后新加的设置项、用户手改过的字段，都不会被这里抹掉。写盘走
//! "临时文件 + rename"，中途断电也不会留下半个 JSON。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::i18n::{Message, Msg};

/// 一条收藏。
///
/// `is_dir` 是**收藏那一刻**的判断，不是每次去问磁盘：
/// 收藏的目录被删掉之后，`path.is_dir()` 会回 `false`，于是点它会去让系统打开
/// 一个不存在的目录，报出来的是"没有应用能打开它"——那是误导。记住当初的判断，
/// 点击时才知道用户收藏的是个目录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// 收藏存在哪儿。与 GUI 的设置文件是同一个。
///
/// 测试要隔离，所以先看 `state_dir_override`（下面那个）：没有覆盖时才走
/// `session::state_dir()`。跟 `volumes::set_override` 是同一套路——
/// **覆盖的是"文件在哪"，不是"收藏存了什么"**，所以判据走的仍是真实读写路径。
pub fn file_path() -> Option<PathBuf> {
    if let Some(dir) = dir_override() {
        return Some(dir.join("settings.json"));
    }
    crate::session::state_dir().map(|dir| dir.join("settings.json"))
}

/// 只给测试用的目录覆盖。
static DIR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn dir_override() -> Option<PathBuf> {
    DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 把收藏的落点指到别处（`None` 恢复默认）。**只给测试用**：
/// 不隔离的话判据会往用户真实的 `settings.json` 里写收藏。
pub fn set_dir_override(dir: Option<PathBuf>) {
    *DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = dir;
}

/// 当前所有收藏（新的在前）。读不到、解析不了都当空——收藏栏空着，
/// 比让整个应用因为一份坏设置起不来强。
pub fn list() -> Vec<Favorite> {
    file_path().map(|path| load_from(&path)).unwrap_or_default()
}

/// 从某个设置文件读收藏。给测试用，也给"用户指定了另一份设置"这种场景留口子。
pub fn load_from(file: &Path) -> Vec<Favorite> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    root.get("favorites")
        .and_then(|value| serde_json::from_value::<Vec<Favorite>>(value.clone()).ok())
        .unwrap_or_default()
}

/// 收藏 / 取消收藏，返回改完之后的整份列表。
///
/// 已经是收藏就删掉，不是就插到最前面（最近收的排最前，跟 GUI 一致）。
pub fn toggle(path: &Path, is_dir: bool) -> Result<Vec<Favorite>, Msg> {
    let Some(file) = file_path() else {
        return Err(Message::msg("找不到设置文件位置"));
    };
    toggle_in(&file, path, is_dir)
}

/// [`toggle`] 的"指定文件"版本。
pub fn toggle_in(file: &Path, path: &Path, is_dir: bool) -> Result<Vec<Favorite>, Msg> {
    let mut root = read_object(file)?;
    let mut items = take(&root);
    match items.iter().position(|item| item.path == path) {
        Some(position) => {
            items.remove(position);
        }
        None => items.insert(
            0,
            Favorite {
                path: path.to_path_buf(),
                is_dir,
            },
        ),
    }
    put(&mut root, &items)?;
    write(file, &root)?;
    Ok(items)
}

/// 收藏这一项在不在里面。界面画"已收藏"标记时用，省得自己再比一遍路径。
pub fn contains(items: &[Favorite], path: &Path) -> bool {
    items.iter().any(|item| item.path == path)
}

/// 整份 JSON 对象；文件不存在就是空的 `{}`（第一次收藏别失败）。
fn read_object(file: &Path) -> Result<serde_json::Map<String, serde_json::Value>, Msg> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(error) => return Err(Message::caused("读不了设置：{error}", error)),
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        // 空文件、或者被手改坏了：**当成空设置**继续。收藏功能不该因为
        // 别人的半行垃圾就整个不可用。
        Ok(_) | Err(_) => Ok(serde_json::Map::new()),
    }
}

fn take(root: &serde_json::Map<String, serde_json::Value>) -> Vec<Favorite> {
    root.get("favorites")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn put(
    root: &mut serde_json::Map<String, serde_json::Value>,
    items: &[Favorite],
) -> Result<(), Msg> {
    let value = serde_json::to_value(items)
        .map_err(|error| Message::caused("序列化收藏失败：{error}", error.to_string()))?;
    root.insert("favorites".to_string(), value);
    Ok(())
}

fn write(file: &Path, root: &serde_json::Map<String, serde_json::Value>) -> Result<(), Msg> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(Message::from)?;
    }
    let json = serde_json::to_string_pretty(root)
        .map_err(|error| Message::caused("序列化设置失败：{error}", error.to_string()))?;
    // 先写临时文件再改名：写到一半断电也不会留下半个 JSON。
    let temp = file.with_extension("json.tmp");
    std::fs::write(&temp, json.as_bytes()).map_err(Message::from)?;
    std::fs::rename(&temp, file).map_err(Message::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        (dir, file)
    }

    #[test]
    fn missing_file_is_an_empty_list() {
        let (_dir, file) = temp_file();
        assert!(load_from(&file).is_empty());
    }

    #[test]
    fn toggle_adds_then_removes() {
        let (_dir, file) = temp_file();

        let after_add = toggle_in(&file, Path::new("/tmp/photos"), true).unwrap();
        assert_eq!(after_add.len(), 1);
        assert_eq!(after_add[0].path, PathBuf::from("/tmp/photos"));
        assert!(after_add[0].is_dir, "收藏那一刻是目录，这个判断要留下来");

        let after_remove = toggle_in(&file, Path::new("/tmp/photos"), true).unwrap();
        assert!(after_remove.is_empty(), "再点一次就是取消收藏");
        assert!(load_from(&file).is_empty(), "写回文件了才算数");
    }

    #[test]
    fn newest_comes_first() {
        let (_dir, file) = temp_file();
        toggle_in(&file, Path::new("/tmp/a"), true).unwrap();
        let items = toggle_in(&file, Path::new("/tmp/b"), false).unwrap();
        assert_eq!(items[0].path, PathBuf::from("/tmp/b"));
        assert_eq!(items[1].path, PathBuf::from("/tmp/a"));
    }

    /// 这条是这套实现的核心约束：**只动 `favorites` 一个键**。
    /// 界面新加的设置项（这里用 `show_hidden` 冒充）不能被收藏写坏。
    #[test]
    fn saving_keeps_every_other_setting() {
        let (_dir, file) = temp_file();
        std::fs::write(
            &file,
            r#"{"show_hidden":true,"term":"iterm","nested":{"a":1}}"#,
        )
        .unwrap();

        toggle_in(&file, Path::new("/tmp/keep"), true).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(root["show_hidden"], serde_json::Value::Bool(true));
        assert_eq!(root["term"], serde_json::Value::String("iterm".into()));
        assert_eq!(root["nested"]["a"], serde_json::Value::from(1));
        assert_eq!(root["favorites"].as_array().unwrap().len(), 1);
    }

    /// 设置文件被手改坏了，收藏功能不该跟着废掉——按"没有收藏"继续。
    #[test]
    fn broken_json_reads_as_empty_and_can_still_be_written() {
        let (_dir, file) = temp_file();
        std::fs::write(&file, "{not json").unwrap();
        assert!(load_from(&file).is_empty());

        let items = toggle_in(&file, Path::new("/tmp/x"), false).unwrap();
        assert_eq!(items.len(), 1);
        // 写完必须是合法 JSON（坏内容本来就没有可保留的信息）。
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
    }

    /// 覆盖只改"文件在哪"，读写路径本身不许绕开——否则判据验的是另一套代码。
    #[test]
    fn dir_override_moves_the_file_not_the_logic() {
        let dir = tempfile::tempdir().unwrap();
        set_dir_override(Some(dir.path().to_path_buf()));

        let items = toggle(Path::new("/tmp/via-override"), true).unwrap();
        assert_eq!(items.len(), 1, "覆盖之后 toggle 走的仍是同一套读写");
        assert!(
            dir.path().join("settings.json").exists(),
            "文件落在覆盖目录里"
        );
        assert_eq!(list().len(), 1, "list 也认同一个落点");

        set_dir_override(None);
    }

    #[test]
    fn contains_matches_the_whole_path() {
        let items = vec![Favorite {
            path: PathBuf::from("/tmp/a"),
            is_dir: true,
        }];
        assert!(contains(&items, Path::new("/tmp/a")));
        assert!(!contains(&items, Path::new("/tmp/ab")));
    }
}
