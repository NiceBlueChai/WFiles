//! 地址栏里敲进来的那句路径 → 该去哪儿。
//!
//! 三件事分开，各自可测：
//! - [`resolve`] 把一句话分成四种结局（什么都不做 / 进目录 / 定位文件 / 找不到）；
//! - `absolute` 把它变成绝对路径（`~` 展开、相对路径接到当前目录、`.`/`..` 按字面收掉）；
//! - 真正读盘只有一处 `fs::metadata`，只用来判定"是目录、是文件、还是不存在"。
//!
//! **明确不做**，免得以后有人当成漏了：不认 `$VAR`、不认通配符、不认
//! `file://`、不做 `~user`（`~user` 当普通目录名）。这几样每一样都要一套
//! 解析规则，而它们在地址栏里的出现频率远低于"把一条路径粘进来"——
//! 宁可把粘贴那条路做扎实（见 `unquote`），也不要在地址栏里造一个半吊子
//! 的 shell。
//!
//! **为什么不 canonicalize**：那会把用户给的那条路径换成另一个
//! （`/tmp` → `/private/tmp`），地址栏里显示出来的就不再是他刚敲的那一条。
//! 这里只做字面收缩，符号链接下面 `..` 的语义差异留给系统去解释。

use std::path::{Component, Path, PathBuf};

/// 一句话该去哪儿。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 空白输入：什么都不做（回车只是收起输入框）。
    Nothing,
    /// 是个目录：过去。
    Directory(PathBuf),
    /// 是个文件：去它的父目录，并把它选中。
    ///
    /// 为什么不是"直接打开"：地址栏是**导航**用的，"前往某个文件"
    /// 在系统文件管理器里的含义一向是"带我去看它在哪儿"。
    File(PathBuf),
    /// 不存在或读不到：不导航，并把用户敲的那句留着让他改。
    Missing(PathBuf),
}

/// 把用户敲的一句话解析成去处。`cwd` 是当前目录（相对路径的参照）。
pub fn resolve(input: &str, cwd: &Path) -> Target {
    resolve_with(input, cwd, dirs::home_dir().as_deref())
}

/// [`resolve`] 但家目录从外面传：`~` 的结果要能钉住，
/// 不能取决于跑测试那台机器的家目录。
pub fn resolve_with(input: &str, cwd: &Path, home: Option<&Path>) -> Target {
    let Some(text) = unquote(input) else {
        return Target::Nothing;
    };
    let path = absolute(text, cwd, home);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Target::Directory(path),
        Ok(_) => Target::File(path),
        Err(_) => Target::Missing(path),
    }
}

/// 去掉首尾空白与成对引号；只剩空白就返回 `None`（= 什么都不做）。
///
/// 引号这一手是为粘贴准备的：从终端、从脚本、从聊天窗口里拷出来的路径
/// 十有八九带着引号（路径里有空格时更是必然），带着引号去查当然找不到，
/// 而用户看到的是"我明明粘对了"。`"…"` 与 `'…'` 都认。
fn unquote(input: &str) -> Option<&str> {
    let text = input.trim();
    let text = strip_pair(text, '"')
        .or_else(|| strip_pair(text, '\''))
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then_some(text)
}

fn strip_pair(text: &str, quote: char) -> Option<&str> {
    text.strip_prefix(quote)?.strip_suffix(quote)
}

/// 一句话 → 绝对路径。不碰文件系统。
fn absolute(text: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = home_relative(text, home) {
        return collapse(&rest);
    }
    let candidate = Path::new(text);
    if candidate.is_absolute() {
        collapse(candidate)
    } else {
        // `join` 顺带管了 Windows 上 `\Windows` 这种写法（"当前盘符的根"）：
        // 有根、没前缀的路径会替换掉前缀之后的部分——Rust 文档写明了这一点，
        // 所以这里不必自己去取盘符。
        collapse(&cwd.join(candidate))
    }
}

/// `~` / `~/x` → 家目录底下；别的写法（`~user`）返回 `None`，当普通路径。
fn home_relative(text: &str, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    if text == "~" {
        return Some(home.to_path_buf());
    }
    let rest = text.strip_prefix("~/").or_else(|| {
        if cfg!(windows) {
            text.strip_prefix("~\\")
        } else {
            None
        }
    })?;
    Some(home.join(rest))
}

/// 按字面收掉 `.` 与 `..`：`a/../b` → `b`，`/..` 还是 `/`。
fn collapse(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 根上面没有上一级：`/..` 就停在 `/`。判据落在"最后一段是不是
                // 普通名字"上——是的话才 pop，不然会把 `/` 或 `C:` 弹掉。
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("临时目录")
    }

    #[test]
    fn blank_input_does_nothing() {
        let home = Path::new("/home/me");
        for input in ["", "   ", "\"\"", "  ''  ", "\t"] {
            assert_eq!(
                resolve_with(input, Path::new("/tmp"), Some(home)),
                Target::Nothing,
                "{input:?} 应该是「什么都不做」"
            );
        }
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let home = Path::new("/home/me");
        let cwd = Path::new("/somewhere/else");
        assert_eq!(
            resolve_with("~", cwd, Some(home)),
            Target::Missing(PathBuf::from("/home/me")),
            "`~` 该展开成家目录（不存在就是 Missing，但要展开过）"
        );
        assert_eq!(
            resolve_with("~/文档", cwd, Some(home)),
            Target::Missing(PathBuf::from("/home/me/文档")),
        );
        // `~user` 不做：照字面当目录名，所以是在当前目录下面找。
        assert_eq!(
            resolve_with("~root", cwd, Some(home)),
            Target::Missing(PathBuf::from("/somewhere/else/~root")),
        );
        // 连家目录都不知道在哪儿时（极端情况）也不许崩。
        assert_eq!(
            resolve_with("~/x", cwd, None),
            Target::Missing(PathBuf::from("/somewhere/else/~/x")),
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_current_directory() {
        let tmp = dir();
        let cwd = tmp.path();
        std::fs::create_dir(cwd.join("sub")).expect("造目录");
        std::fs::write(cwd.join("a.txt"), "x").expect("造文件");

        assert_eq!(
            resolve_with("sub", cwd, None),
            Target::Directory(cwd.join("sub"))
        );
        assert_eq!(
            resolve_with("./sub/", cwd, None),
            Target::Directory(cwd.join("sub")),
            "`.` 与结尾的斜杠都该收掉"
        );
        assert_eq!(
            resolve_with("a.txt", cwd, None),
            Target::File(cwd.join("a.txt"))
        );
        // 上一层再进别的目录：`..` 必须真的收掉，不然地址栏里会留着它。
        assert_eq!(
            resolve_with("sub/../sub", cwd, None),
            Target::Directory(cwd.join("sub"))
        );
    }

    #[test]
    fn parent_directories_collapse_lexically() {
        let cwd = Path::new("/a/b/c");
        assert_eq!(
            resolve_with("../..", cwd, None),
            Target::Missing(PathBuf::from("/a")),
        );
        // 根上面还是根：`/..` 不许把 `/` 弹没。
        assert_eq!(absolute("/..", cwd, None), PathBuf::from("/"));
        assert_eq!(absolute("/../..", cwd, None), PathBuf::from("/"));
    }

    #[test]
    fn quoted_paths_are_unwrapped() {
        let tmp = dir();
        let cwd = tmp.path();
        let spaced = cwd.join("带空格的 目录");
        std::fs::create_dir(&spaced).expect("造目录");

        for input in [
            format!("\"{}\"", spaced.display()),
            format!("'{}'", spaced.display()),
            format!("  \"{}\"  ", spaced.display()),
        ] {
            assert_eq!(
                resolve_with(&input, cwd, None),
                Target::Directory(spaced.clone()),
                "{input} 该当成路径本身，而不是带着引号的名字"
            );
        }
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let tmp = dir();
        assert_eq!(
            resolve_with(&tmp.path().to_string_lossy(), Path::new("/"), None),
            Target::Directory(tmp.path().to_path_buf()),
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_missing_not_an_error() {
        let tmp = dir();
        let missing = tmp.path().join("没有这个");
        assert_eq!(
            resolve_with(&missing.to_string_lossy(), tmp.path(), None),
            Target::Missing(missing),
        );
    }

    /// Windows 上"当前盘符的根"那种写法：`\Users` 不该被当成相对路径。
    ///
    /// 这一条**只在这里编译得到**（macOS/Linux 上 `\` 是合法的文件名字符，
    /// 那种输入的含义完全不同，不能拿来做断言）。
    #[cfg(windows)]
    #[test]
    fn windows_rooted_paths_use_the_current_drive() {
        let cwd = Path::new(r"C:\Users\me");
        std::fs::create_dir_all(r"C:\Windows").ok();
        assert_eq!(
            resolve_with(r"\Windows", cwd, None),
            Target::Directory(PathBuf::from(r"C:\Windows")),
        );
    }
}
