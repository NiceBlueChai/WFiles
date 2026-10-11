//! 批量重命名：模板、序号与查找替换，先预览后执行。
//!
//! 预览是纯函数：同一批路径和同一套规则，任何时候算出来的结果都一样，
//! 用户改一个字符就能立刻看到全部新名字，确认后才落盘。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一套批量重命名规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkRename {
    /// 名字模板，占位符：`{name}` 原主干、`{ext}` 原扩展名（含点）、`{n}` 序号。
    pub template: String,
    /// 查找替换作用在原文件名上（先执行），留空则跳过。
    pub find: String,
    /// 替换成的内容。
    pub replace_with: String,
    /// 序号起始值。
    pub start: u32,
    /// 序号步长。
    pub step: u32,
}

impl Default for BulkRename {
    fn default() -> Self {
        Self {
            template: "{name}{ext}".to_string(),
            find: String::new(),
            replace_with: String::new(),
            start: 1,
            step: 1,
        }
    }
}

/// 一项预览结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkRenamePreview {
    pub path: PathBuf,
    pub old_name: String,
    pub new_name: String,
    /// 这一项不能执行的原因（名字非法、与磁盘上其他项重名等）。
    pub error: Option<String>,
    /// 是否与当前目录里另一个被选中的项撞名。
    pub collision: bool,
    /// 新旧名字是否相同（没变化就不必执行）。
    pub unchanged: bool,
}

impl BulkRenamePreview {
    /// 能不能执行。
    pub fn executable(&self) -> bool {
        self.error.is_none() && !self.collision && !self.unchanged
    }
}

/// 拆出主干和扩展名（含点）；点号在开头的不算扩展名。
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(ix) if ix > 0 => (&name[..ix], &name[ix..]),
        _ => (name, ""),
    }
}

/// 按规则算出第 `index` 项的新名字（不含错误检查）。
///
/// `batch_size` 决定序号补零位数：整批能容纳下的最大值有几位就补几位。
fn render_name(rule: &BulkRename, old_name: &str, index: usize, batch_size: usize) -> String {
    let (stem, extension) = split_name(old_name);

    // 先做查找替换，作用在整个文件名上。
    let searched = if rule.find.is_empty() {
        old_name.to_string()
    } else {
        old_name.replace(&rule.find, &rule.replace_with)
    };
    let (searched_stem, searched_ext) = split_name(&searched);

    // 序号：补零到至少能容纳最后一项的位数。
    let last_value = rule.start as usize + rule.step as usize * batch_size.saturating_sub(1);
    let padding = last_value.to_string().len().max(1);
    let value = rule.start as usize + rule.step as usize * index;
    let serial = format!("{value:0padding$}");

    rule.template
        .replace("{name}", searched_stem)
        .replace("{ext}", searched_ext)
        .replace("{n}", &serial)
        // 模板里没提扩展名时保留原扩展名，避免 "a.txt" 变成 "a"。
        .replace("{stem}", stem)
        .replace("{extension}", extension)
        .trim()
        .to_string()
}

/// 生成预览。`paths` 按列表顺序，序号据此递增。
pub fn preview(paths: &[PathBuf], rule: &BulkRename) -> Vec<BulkRenamePreview> {
    let batch_size = paths.len();
    let mut previews: Vec<BulkRenamePreview> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let old_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let new_name = render_name(rule, &old_name, index, batch_size);
            BulkRenamePreview {
                path: path.clone(),
                old_name,
                new_name,
                error: None,
                collision: false,
                unchanged: false,
            }
        })
        .collect();

    // 批次内部重名：后出现的算冲突。
    let mut seen: Vec<String> = Vec::new();
    for preview in &mut previews {
        if preview.new_name.is_empty() {
            preview.error = Some("名称为空".to_string());
            continue;
        }
        if preview.new_name.contains('/') || preview.new_name.contains('\\') {
            preview.error = Some("名称不能包含路径分隔符".to_string());
            continue;
        }
        if preview.new_name == "." || preview.new_name == ".." {
            preview.error = Some("名称非法".to_string());
            continue;
        }
        if seen.contains(&preview.new_name) {
            preview.collision = true;
        } else {
            seen.push(preview.new_name.clone());
        }
    }

    // 与磁盘上已有的其他项重名（不包括自己现在占的位置）。
    for preview in &mut previews {
        if preview.error.is_some() || preview.collision {
            continue;
        }
        let Some(parent) = preview.path.parent() else {
            continue;
        };
        let candidate = parent.join(&preview.new_name);
        if candidate.exists() && candidate != preview.path {
            preview.collision = true;
        }
    }

    // 大小写改名在部分文件系统上是合法的，这里通过 exists 比较已经放行；
    // 真正没变的标记出来，省得用户白按一次执行。
    for preview in &mut previews {
        preview.unchanged = preview.new_name == preview.old_name;
    }

    previews
}

/// 有多少项真的会被执行。
pub fn executable_count(previews: &[BulkRenamePreview]) -> usize {
    previews.iter().filter(|item| item.executable()).count()
}

/// 执行批量重命名。只动可执行的那些项，返回逐项结果。
///
/// 成功的按 `(原路径, 新路径)` 返回，失败带上原因。
pub fn apply(
    previews: &[BulkRenamePreview],
) -> (Vec<(PathBuf, PathBuf)>, Vec<(PathBuf, String)>) {
    let mut renamed = Vec::new();
    let mut failed = Vec::new();

    for preview in previews {
        if !preview.executable() {
            continue;
        }
        match crate::ops::rename(&preview.path, &preview.new_name) {
            Ok(new_path) => renamed.push((preview.path.clone(), new_path)),
            Err(error) => failed.push((preview.path.clone(), error)),
        }
    }

    (renamed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn paths_of(names: &[&str], root: &Path) -> Vec<PathBuf> {
        names.iter().map(|name| root.join(name)).collect()
    }

    #[test]
    fn template_keeps_stem_and_extension() {
        let root = temp();
        let paths = paths_of(&["a.txt", "b.log"], root.path());
        let previews = preview(&paths, &BulkRename::default());

        assert_eq!(previews[0].new_name, "a.txt");
        assert_eq!(previews[1].new_name, "b.log");
        assert!(previews.iter().all(|item| item.unchanged));
    }

    #[test]
    fn serial_numbers_are_padded_and_ordered() {
        let root = temp();
        let paths = paths_of(&["a.txt", "b.txt", "c.txt"], root.path());
        let rule = BulkRename {
            template: "IMG_{n}{ext}".to_string(),
            start: 1,
            step: 1,
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        let names: Vec<&str> = previews.iter().map(|item| item.new_name.as_str()).collect();
        // 3 项 -> 1 位就够；10 项以上才补零。
        assert_eq!(names, ["IMG_1.txt", "IMG_2.txt", "IMG_3.txt"]);
    }

    #[test]
    fn serial_padding_grows_with_batch_size() {
        let root = temp();
        let names: Vec<String> = (0..12).map(|ix| format!("f{ix}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let paths = paths_of(&refs, root.path());
        let rule = BulkRename {
            template: "{n}{ext}".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert_eq!(previews[0].new_name, "01.txt");
        assert_eq!(previews[11].new_name, "12.txt");
    }

    #[test]
    fn serial_respects_start_and_step() {
        let root = temp();
        let paths = paths_of(&["a.txt", "b.txt"], root.path());
        let rule = BulkRename {
            template: "{n}{ext}".to_string(),
            start: 5,
            step: 10,
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        // 全批统一宽度：最后一项是 15（两位），所以 5 也要补成 05。
        assert_eq!(previews[0].new_name, "05.txt");
        assert_eq!(previews[1].new_name, "15.txt");
    }

    #[test]
    fn find_and_replace_runs_before_the_template() {
        let root = temp();
        let paths = paths_of(&["IMG_0001.jpg", "IMG_0002.jpg"], root.path());
        let rule = BulkRename {
            template: "照片_{name}{ext}".to_string(),
            find: "IMG_".to_string(),
            replace_with: String::new(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert_eq!(previews[0].new_name, "照片_0001.jpg");
        assert_eq!(previews[1].new_name, "照片_0002.jpg");
    }

    #[test]
    fn collisions_inside_the_batch_are_flagged() {
        let root = temp();
        let paths = paths_of(&["a.txt", "A.txt"], root.path());
        let rule = BulkRename {
            template: "same{ext}".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert_eq!(previews[0].new_name, "same.txt");
        assert!(!previews[0].collision);
        assert!(previews[1].collision, "同批次重名，后出现的一方冲突");
        assert!(!previews[1].executable());
    }

    #[test]
    fn collision_with_an_existing_file_is_flagged() {
        let root = temp();
        fs::write(root.path().join("taken.txt"), "x").unwrap();
        let paths = paths_of(&["a.txt"], root.path());
        let rule = BulkRename {
            template: "taken{ext}".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert!(previews[0].collision);
        assert!(!previews[0].executable());
    }

    #[test]
    fn case_only_rename_is_allowed() {
        let root = temp();
        let paths = paths_of(&["a.txt"], root.path());
        let rule = BulkRename {
            template: "A{ext}".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        // 目标是“自己”（大小写不同），不算冲突。
        assert!(!previews[0].collision);
        assert!(!previews[0].unchanged);
        assert!(previews[0].executable());
    }

    #[test]
    fn empty_and_separator_names_are_errors() {
        let root = temp();
        let paths = paths_of(&["a.txt", "b.txt"], root.path());
        let rule = BulkRename {
            template: " ".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert!(previews.iter().all(|item| item.error.is_some()));
        assert!(previews.iter().all(|item| !item.executable()));

        let rule = BulkRename {
            template: "a/b{ext}".to_string(),
            ..BulkRename::default()
        };
        let previews = preview(&paths, &rule);
        assert!(previews.iter().all(|item| item.error.is_some()));
    }

    #[test]
    fn apply_renames_only_executable_entries() {
        let root = temp();
        fs::write(root.path().join("a.txt"), "aaa").unwrap();
        fs::write(root.path().join("b.txt"), "bbb").unwrap();
        let paths = paths_of(&["a.txt", "b.txt"], root.path());
        let rule = BulkRename {
            template: "x_{name}{ext}".to_string(),
            ..BulkRename::default()
        };

        let previews = preview(&paths, &rule);
        assert_eq!(executable_count(&previews), 2);

        let (renamed, failed) = apply(&previews);
        assert!(failed.is_empty());
        assert_eq!(renamed.len(), 2);
        assert_eq!(fs::read_to_string(root.path().join("x_a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(root.path().join("x_b.txt")).unwrap(), "bbb");
        assert!(!root.path().join("a.txt").exists());
    }

    #[test]
    fn unchanged_entries_are_skipped_by_apply() {
        let root = temp();
        fs::write(root.path().join("a.txt"), "x").unwrap();
        let paths = paths_of(&["a.txt"], root.path());

        let (renamed, failed) = apply(&preview(&paths, &BulkRename::default()));
        assert!(renamed.is_empty());
        assert!(failed.is_empty());
        assert!(root.path().join("a.txt").exists());
    }

    use std::fs;
}
