//! 可配置的快捷键：命令 ↔ 按键，以及"改了之后怎么存"。
//!
//! 这一层不认识 GPUI，只有字符串与表，判定规则可以单独测；
//! [`crate::pane::init`] 负责把这里给出的表交给 GPUI 去绑。
//!
//! 一条命令出厂时可以有多条按键（"打开"既是 Enter 也是 Cmd+↓），
//! 但用户自己指定时只认一条：想配两条的用户是个位数，
//! 为此把配置格式和界面都搞复杂不值得。
//!
//! 改动**下次启动才完全生效**：GPUI 只有 `bind_keys` 没有"解绑"，
//! 运行期改掉一条，旧的那个键还挂在匹配器里摘不下来。
//! 所以这里只负责算清楚"生效的绑定表"，启动时一次性绑好。

use std::collections::BTreeMap;

use crate::pane::PaneCommand;

/// 用户改过的按键：命令 id → 按键写法。只存改过的，没改的就是出厂的。
///
/// 用 `BTreeMap` 而不是 `HashMap`：写出来的配置文件按命令名排序，
/// 用户打开 settings.json 时找得到。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyMap(pub BTreeMap<String, String>);

impl KeyMap {
    /// 这条命令现在按哪个键。没改过就报出厂的第一条。
    pub fn key_for(&self, command: PaneCommand) -> String {
        self.0
            .get(command.id())
            .cloned()
            .or_else(|| defaults(command).first().map(|key| key.to_string()))
            .unwrap_or_default()
    }

    /// 记下这条命令的按键。
    pub fn set(&mut self, command: PaneCommand, key: String) {
        self.0.insert(command.id().to_string(), key);
    }

    /// 恢复出厂按键（从表里删掉就是了）。
    pub fn clear(&mut self, command: PaneCommand) {
        self.0.remove(command.id());
    }

    /// 启动时真正要绑的那张表：改过的用改的，没改的用出厂那一组。
    pub fn bindings(&self) -> Vec<(PaneCommand, String)> {
        let mut bindings = Vec::new();
        for command in PaneCommand::ALL {
            match self.0.get(command.id()) {
                Some(key) => bindings.push((command, key.clone())),
                None => bindings.extend(
                    defaults(command)
                        .iter()
                        .map(|key| (command, key.to_string())),
                ),
            }
        }
        bindings
    }

    /// 丢掉认不出来的条目：手改配置文件写错命令名是常事，
    /// 留着它们只会让"改了没生效"更难排查。
    pub fn drop_unknown(&mut self) {
        self.0.retain(|id, _| PaneCommand::from_id(id).is_some());
    }

    /// 当前设置里那份。
    pub fn read(cx: &gpui_kit::App) -> Self {
        let mut map = crate::settings::Settings::read(cx).keys;
        map.drop_unknown();
        map
    }

    /// 写回设置并落盘。
    pub fn save(&self, cx: &mut gpui_kit::App) {
        let map = self.clone();
        crate::settings::Settings::update(cx, |settings| settings.keys = map);
    }
}

/// 这条命令出厂时绑在哪些键上。
pub fn defaults(command: PaneCommand) -> &'static [&'static str] {
    match command {
        PaneCommand::NewFolder => &["cmd-shift-n"],
        PaneCommand::Rename => &["f2"],
        PaneCommand::BulkRename => &["cmd-shift-r"],
        PaneCommand::Trash => &["cmd-backspace"],
        PaneCommand::Copy => &["cmd-c"],
        PaneCommand::Cut => &["cmd-x"],
        PaneCommand::Paste => &["cmd-v"],
        PaneCommand::Undo => &["cmd-z"],
        PaneCommand::Refresh => &["cmd-r"],
        PaneCommand::GoUp => &["backspace", "cmd-up"],
        PaneCommand::CopyPath => &["cmd-alt-c"],
        PaneCommand::CopyFileName => &["cmd-shift-c"],
        PaneCommand::OpenTerminal => &["cmd-alt-t"],
        PaneCommand::QuickLook => &["space"],
        PaneCommand::ViewArchive => &[],
        PaneCommand::CreateArchive => &[],
        PaneCommand::ViewList => &["cmd-1"],
        PaneCommand::ViewGrid => &["cmd-2"],
        PaneCommand::ViewColumns => &["cmd-3"],
    }
}

/// 这个按键写法能不能拿去绑。
///
/// 只挡两类：空串，以及没有实际按键、光按了修饰键的组合
/// （"cmd-shift-" 解出来没有任何意义，绑上去等于把命令废掉）。
/// 其余交给 GPUI 的 `Keystroke::parse`——它认得的就能绑，认不得的绑了也没人按得出来。
pub fn is_usable(key: &str) -> bool {
    if key.trim().is_empty() {
        return false;
    }
    let parts: Vec<&str> = key.split('-').collect();
    // 最后一段是真正的键；前面每段都得是修饰键名。
    let (last, modifiers) = parts.split_last().expect("非空");
    !last.is_empty()
        && modifiers.iter().all(|part| is_modifier(part))
}

fn is_modifier(part: &str) -> bool {
    [
        "ctrl", "alt", "shift", "fn", "cmd", "super", "win", "secondary", "control", "platform",
    ]
    .contains(&part.to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::{KeyMap, defaults, is_usable};
    use crate::pane::PaneCommand;

    #[test]
    fn every_command_has_an_id_that_round_trips() {
        for command in PaneCommand::ALL {
            assert_eq!(
                PaneCommand::from_id(command.id()),
                Some(command),
                "{} 的 id 要能解析回自己",
                command.id()
            );
        }
    }

    #[test]
    fn an_unknown_id_is_rejected() {
        assert_eq!(PaneCommand::from_id("不存在的命令"), None);
        assert_eq!(PaneCommand::from_id(""), None);
    }

    #[test]
    fn defaults_are_used_until_something_is_set() {
        let mut map = KeyMap::default();
        assert_eq!(map.key_for(PaneCommand::Rename), "f2");

        map.set(PaneCommand::Rename, "cmd-shift-k".to_string());
        assert_eq!(map.key_for(PaneCommand::Rename), "cmd-shift-k");

        // 改过的取代出厂那一组，而不是和它并存。
        let bound = map.bindings();
        assert_eq!(
            bound
                .iter()
                .filter(|(command, _)| *command == PaneCommand::Rename)
                .count(),
            1
        );

        map.clear(PaneCommand::Rename);
        assert_eq!(map.key_for(PaneCommand::Rename), "f2", "清掉就回出厂");
    }

    #[test]
    fn a_command_with_two_defaults_binds_both() {
        let map = KeyMap::default();
        let keys: Vec<_> = map
            .bindings()
            .into_iter()
            .filter(|(command, _)| *command == PaneCommand::GoUp)
            .map(|(_, key)| key)
            .collect();
        assert_eq!(keys, ["backspace", "cmd-up"]);
    }

    #[test]
    fn the_binding_table_covers_every_command_that_has_a_key() {
        let map = KeyMap::default();
        let bound: Vec<_> = map.bindings().into_iter().map(|(command, _)| command).collect();
        for command in PaneCommand::ALL {
            if defaults(command).is_empty() {
                assert!(!bound.contains(&command), "{command:?} 出厂没键，不该凭空绑一个");
            } else {
                assert!(bound.contains(&command), "{command:?} 该绑上");
            }
        }
    }

    #[test]
    fn a_key_without_an_actual_key_is_refused() {
        assert!(is_usable("f2"));
        assert!(is_usable("cmd-shift-k"));
        assert!(is_usable("enter"));
        assert!(!is_usable(""), "空串");
        assert!(!is_usable("cmd-shift-"), "光有修饰键");
        assert!(!is_usable("   "), "空白");
    }

    #[test]
    fn hand_edited_junk_is_dropped_not_kept() {
        let mut map = KeyMap::default();
        map.set(PaneCommand::Rename, "f3".to_string());
        map.0.insert("手滑打错的命令".to_string(), "f4".to_string());
        map.drop_unknown();
        assert_eq!(map.0.len(), 1, "认不出来的条目该丢掉");
        assert_eq!(map.key_for(PaneCommand::Rename), "f3");
    }

    #[test]
    fn a_keymap_survives_the_roundtrip() {
        let mut map = KeyMap::default();
        map.set(PaneCommand::Trash, "cmd-shift-delete".to_string());
        let json = serde_json::to_string(&map).unwrap();
        let back: KeyMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);

        // 旧配置里没有这个字段时走默认（全出厂）。
        let back: KeyMap = serde_json::from_str("{}").unwrap();
        assert!(back.0.is_empty());
    }
}
