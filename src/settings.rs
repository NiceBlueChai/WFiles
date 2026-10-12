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

use crate::terminal;

/// 需要跨重启记住的偏好。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 打开终端用哪个程序；`terminal::AUTO` 表示跟着系统走。
    #[serde(default = "default_terminal")]
    pub terminal: String,
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

    #[test]
    fn roundtrip_preserves_the_terminal_choice() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("settings.json");

        let settings = Settings {
            terminal: "iterm".to_string(),
        };
        settings.save_to(&path).unwrap();

        assert_eq!(Settings::load_from(&path), Some(settings));
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
