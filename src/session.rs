//! 会话持久化：记住开了哪些标签、各自在看哪个目录，下次启动原样恢复。
//!
//! 存不下来不应当影响启动——任何读取/解析失败都退回默认会话（一个主页标签）。

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// 需要跨重启记住的全部状态。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// 按打开顺序排列的标签目录。
    pub tabs: Vec<PathBuf>,
    /// 当前标签在 `tabs` 里的下标。
    pub active: usize,
}

impl Session {
    /// 读取默认位置的会话；失败时返回默认值。
    pub fn load() -> Self {
        Self::file_path()
            .and_then(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 写入默认位置。目录不存在时一并创建。
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::file_path() else {
            return Err("找不到会话文件位置".to_string());
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        // 先写临时文件再改名，避免半途崩溃留下写一半的会话。
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(json.as_bytes())
            .map_err(|error| error.to_string())?;
        fs::rename(&temp, path).map_err(|error| error.to_string())
    }

    /// 会话文件位置：优先 `WFILES_SESSION_DIR` 指向的目录，
    /// 否则按平台惯例落到 state/config/home 下的 `WFiles/session.json`。
    ///
    /// 环境变量不是歪门邪道：便携安装、多开实例各存一份，
    /// 以及集成测试要把会话隔到临时目录里，都靠它。
    fn file_path() -> Option<PathBuf> {
        if let Some(dir) = session_dir_override() {
            return Some(dir.join("session.json"));
        }
        dirs::state_dir()
            .or_else(dirs::config_dir)
            .or_else(dirs::home_dir)
            .map(|dir| dir.join("WFiles").join("session.json"))
    }

    /// 恢复前的清洗：丢掉已经不存在的目录，修正越界的当前标签。
    ///
    /// 标签全部失效时留下一个空列表，由调用方决定补一个默认位置。
    pub fn sanitized(mut self) -> Self {
        self.tabs
            .retain(|path| path.is_dir() && path.parent().is_some());
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self
    }
}

/// 读取会话目录覆盖；空字符串视为没设置。
fn session_dir_override() -> Option<PathBuf> {
    let raw = std::env::var("WFILES_SESSION_DIR").ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_tabs_and_active() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("session.json");

        let session = Session {
            tabs: vec![PathBuf::from("/Users"), PathBuf::from("/tmp")],
            active: 1,
        };
        session.save_to(&path).unwrap();

        let loaded = Session::load_from(&path).expect("reload");
        assert_eq!(loaded, session);
    }

    #[test]
    fn missing_file_yields_default() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(Session::load_from(&root.path().join("nope.json")), None);
        assert_eq!(Session::load(), Session::default());
    }

    #[test]
    fn corrupt_file_yields_default() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.json");
        fs::write(&path, "{ 这不是 json").unwrap();

        assert_eq!(Session::load_from(&path), None);
    }

    #[test]
    fn sanitize_drops_missing_directories_and_clamps_active() {
        let root = tempfile::tempdir().unwrap();
        let alive = root.path().join("alive");
        fs::create_dir(&alive).unwrap();

        let session = Session {
            tabs: vec![alive.clone(), root.path().join("gone")],
            active: 7,
        }
        .sanitized();

        assert_eq!(session.tabs, vec![alive]);
        assert_eq!(session.active, 0);
    }

    #[test]
    fn sanitize_keeps_files_out() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs::write(&file, "x").unwrap();

        let session = Session { tabs: vec![file], active: 0 }.sanitized();
        assert!(session.tabs.is_empty());
        assert_eq!(session.active, 0);
    }

    #[test]
    fn empty_tabs_survive_sanitize() {
        let session = Session { tabs: Vec::new(), active: 3 }.sanitized();
        assert_eq!(session.active, 0);
    }
}
