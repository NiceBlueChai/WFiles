//! 剪贴板文本的拼装：复制路径、复制文件名共用的一套纯逻辑。
//!
//! 放在独立模块里一是为了能单测，二是 pane.rs 里已经有 GPUI 的重宏展开，
//! 再塞 `#[test]` 会把编译器的宏递归顶爆。

use std::path::{Path, PathBuf};

/// 复制到剪贴板时要完整路径还是只要文件名。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardText {
    FullPath,
    NameOnly,
}

/// 把选中的路径拼成剪贴板文本：每行一个，顺序与选择顺序一致。
///
/// 多选时刻意用换行分隔——粘到终端里就是一组合法的路径参数，
/// 粘到编辑器里就是一个清单，两种场景都成立。
pub fn clipboard_text(paths: &[PathBuf], mode: ClipboardText) -> String {
    paths
        .iter()
        .map(|path| match mode {
            ClipboardText::FullPath => path.display().to_string(),
            ClipboardText::NameOnly => name_of(path),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 取文件名；根目录这类没有文件名的路径退回完整路径，不留空白行。
///
/// 不只剪贴板用，比较/分析面板的标题也要显示"某个文件"而不是一长串路径。
pub fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_keeps_one_entry_per_line() {
        let paths = vec![
            PathBuf::from("/tmp/a.txt"),
            PathBuf::from("/tmp/b.txt"),
            PathBuf::from("/tmp/c.txt"),
        ];
        assert_eq!(
            clipboard_text(&paths, ClipboardText::FullPath),
            "/tmp/a.txt\n/tmp/b.txt\n/tmp/c.txt"
        );
    }

    #[test]
    fn name_only_drops_the_directory() {
        let paths = vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")];
        assert_eq!(
            clipboard_text(&paths, ClipboardText::NameOnly),
            "a.txt\nb.txt"
        );
    }

    #[test]
    fn name_only_falls_back_to_the_full_path() {
        // "/" 没有文件名，退回完整路径而不是空白。
        let paths = vec![PathBuf::from("/")];
        assert_eq!(clipboard_text(&paths, ClipboardText::NameOnly), "/");
    }

    #[test]
    fn empty_selection_yields_an_empty_string() {
        assert_eq!(clipboard_text(&[], ClipboardText::FullPath), "");
        assert_eq!(clipboard_text(&[], ClipboardText::NameOnly), "");
    }

    #[test]
    fn names_with_spaces_survive_round_trip() {
        let paths = vec![PathBuf::from("/tmp/我的 文件.txt")];
        assert_eq!(
            clipboard_text(&paths, ClipboardText::NameOnly),
            "我的 文件.txt"
        );
    }
}
