//! 命令行：从外面把应用调起来。
//!
//! 只做"开到哪儿去"这一件事：打开一个目录、定位一个文件。**不做无界面的
//! 文件操作**（`wfiles trash 某个文件` 那种）——删除、覆盖在界面上是
//! "确认 + 进度 + 可撤销"一起给的，从命令行绕过去等于给破坏性操作开出
//! 第二条路，而第二条路迟早跟第一条走岔（见 ROADMAP「信任与安全」）。
//!
//! **看得懂就认、看不懂就当没有**：从文件管理器、别的应用、脚本里启动时，
//! 参数不由用户控制，为一个没写对的开关拒绝启动是帮倒忙。
//!
//! 解析这一层不碰文件系统也不读环境：路径存不存在、是文件还是目录，
//! 由调用方问清楚了喂进来（[`OnDisk`]）——那个判据到开窗那一刻才有意义，
//! 而这一层能拿三个枚举值把规则测干净。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::i18n;

/// 磁盘上这个路径是什么。
///
/// 从外面喂进来而不是自己去问：解析这一层是纯的，而"是目录还是文件"
/// 只决定"怎么开"，不决定"认不认这个参数"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnDisk {
    Dir,
    File,
    /// 不在了，或者压根没存在过。
    Missing,
}

/// 命令行说要去哪儿。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 裸路径：是目录就开它，是文件就定位到它。
    Open(PathBuf),
    /// `--reveal`：一律定位——开到父目录并选中它。
    Reveal(PathBuf),
}

impl Target {
    pub fn path(&self) -> &Path {
        match self {
            Self::Open(path) | Self::Reveal(path) => path,
        }
    }

    /// 开哪个目录、要不要选中什么。
    ///
    /// 裸路径给目录就开它；给文件就是"定位"的意思（跟 `--reveal` 一个
    /// 效果，这也是系统文件管理器对"用本程序打开一个文件"的惯例）。
    /// 路径不在了就照样开过去，让窗格自己报"打不开"——那儿有错误页，
    /// 比这里默默退回主页更接近用户刚才说的话。
    pub fn resolve(&self, on_disk: OnDisk) -> Opening {
        match self {
            Self::Open(path) => match on_disk {
                OnDisk::Dir => Opening {
                    dir: path.clone(),
                    select: None,
                },
                OnDisk::File => Opening {
                    dir: here(path),
                    select: Some(path.clone()),
                },
                OnDisk::Missing => Opening {
                    dir: path.clone(),
                    select: None,
                },
            },
            // 定位这件事跟存不存在无关：开父目录、选中它本身。
            // 它要是不在列表里，那就只是"什么都没选中"，那也是对的。
            Self::Reveal(path) => Opening {
                dir: here(path),
                select: Some(path.clone()),
            },
        }
    }
}

/// 落到窗格上的样子：开哪个目录，选中哪一个。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opening {
    pub dir: PathBuf,
    pub select: Option<PathBuf>,
}

/// 父目录。
///
/// 相对路径的父目录是**空串**（`报告.txt` 的父目录是 `""`），那意思是
/// "当前目录"，不是"没有目录"——直接拿去开窗会开在一个空路径上。
fn here(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => PathBuf::from("."),
        Some(parent) => parent.to_path_buf(),
        // 根目录自己没有父目录：开它本身。
        None => path.to_path_buf(),
    }
}

/// 一次启动用得上的全部命令行信息。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cli {
    /// `--lang`：这一次启动用什么语言。
    pub locale: Option<i18n::Locale>,
    /// 要开到哪儿；没给就照上次的会话恢复。
    pub launch: Option<Launch>,
}

/// 启动目标，以及开法。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub target: Target,
    /// `--new-tab`：另开一个标签页，而不是把当前那一栏挪过去。
    pub new_tab: bool,
}

/// 认识的就这么几个：`--lang`（`=` 与空格两种写法）、`--reveal`、
/// `--new-tab`，以及一条路径。别的放过。
pub fn parse<I>(args: I) -> Cli
where
    I: IntoIterator<Item = OsString>,
{
    let mut cli = Cli::default();
    let mut path: Option<PathBuf> = None;
    let mut reveal = false;
    let mut new_tab = false;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // 开关都是 ASCII，认不出来的是路径——**路径必须按字节走**：
        // Windows 上不是合法 UTF-8 的文件名是常态。
        let Some(text) = arg.to_str() else {
            keep_path(&mut path, &arg);
            continue;
        };
        if let Some(code) = text.strip_prefix("--lang=") {
            cli.locale = i18n::Locale::parse(code);
        } else if text == "--lang" {
            cli.locale = args
                .next()
                .and_then(|code| i18n::Locale::parse(code.to_str()?));
        } else if text == "--reveal" {
            reveal = true;
        } else if text == "--new-tab" {
            new_tab = true;
        } else if text.starts_with('-') {
            // 认不出来的开关：默默放过，别报错退出。
        } else {
            keep_path(&mut path, &arg);
        }
    }

    cli.launch = path.map(|path| Launch {
        target: if reveal {
            Target::Reveal(path)
        } else {
            Target::Open(path)
        },
        // 没给路径就没有"新的那一栏"可言，这个开关也就没了着落。
        new_tab,
    });
    cli
}

/// 第一个像路径的参数说了算：一次启动只开一个地方。
fn keep_path(slot: &mut Option<PathBuf>, arg: &OsString) {
    if slot.is_none() {
        *slot = Some(PathBuf::from(arg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn a_bare_directory_opens_itself() {
        let opening = Target::Open(PathBuf::from("/tmp/某个目录")).resolve(OnDisk::Dir);
        assert_eq!(opening.dir, PathBuf::from("/tmp/某个目录"));
        assert_eq!(opening.select, None, "开目录不是去选中它");
    }

    #[test]
    fn a_bare_file_opens_its_parent_and_selects_it() {
        let opening = Target::Open(PathBuf::from("/tmp/报告.txt")).resolve(OnDisk::File);
        assert_eq!(opening.dir, PathBuf::from("/tmp"));
        assert_eq!(opening.select, Some(PathBuf::from("/tmp/报告.txt")));
    }

    #[test]
    fn reveal_selects_the_path_itself_even_when_it_is_a_directory() {
        let opening = Target::Reveal(PathBuf::from("/tmp/子目录")).resolve(OnDisk::Dir);
        assert_eq!(opening.dir, PathBuf::from("/tmp"));
        assert_eq!(opening.select, Some(PathBuf::from("/tmp/子目录")));
    }

    #[test]
    fn a_missing_path_still_opens_where_it_was_asked() {
        // 不在了也开过去：窗格会报"打不开"，那是用户要的答案。
        // 退回主页等于把"我刚才说的那条路径"悄悄扔了。
        let opening = Target::Open(PathBuf::from("/tmp/已经没了")).resolve(OnDisk::Missing);
        assert_eq!(opening.dir, PathBuf::from("/tmp/已经没了"));
        assert_eq!(opening.select, None);
    }

    #[test]
    fn a_relative_path_lands_in_the_current_directory() {
        // `报告.txt` 的父目录是空串。拿空串去开窗等于开在一个空路径上，
        // 得翻成"当前目录"。
        let opening = Target::Open(PathBuf::from("报告.txt")).resolve(OnDisk::File);
        assert_eq!(opening.dir, PathBuf::from("."));
        assert_eq!(opening.select, Some(PathBuf::from("报告.txt")));
    }

    #[test]
    fn flags_may_come_before_or_after_the_path() {
        // 两种写法脚本里都有人写，结果得一样。
        for args in [
            vec!["--reveal", "/tmp/x"],
            vec!["/tmp/x", "--reveal"],
            vec!["--reveal", "--new-tab", "/tmp/x"],
        ] {
            let launch = cli(&args).launch.expect("该认出目标");
            assert_eq!(launch.target, Target::Reveal(PathBuf::from("/tmp/x")));
        }
        assert!(cli(&["--new-tab", "/tmp/x"]).launch.unwrap().new_tab);
        assert!(!cli(&["/tmp/x"]).launch.unwrap().new_tab);
    }

    #[test]
    fn unknown_switches_are_ignored_not_fatal() {
        // 从这个应用、别的应用、脚本里启动时，参数不由用户控制。
        let launch = cli(&["--whatever", "-v", "/tmp/x"]).launch.unwrap();
        assert_eq!(launch.target, Target::Open(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn lang_takes_both_spellings_and_stays_out_of_the_way() {
        assert_eq!(cli(&["--lang=en"]).locale, Some(i18n::Locale::En));
        assert_eq!(cli(&["--lang", "en"]).locale, Some(i18n::Locale::En));
        assert_eq!(
            cli(&["--lang", "en", "/tmp/x"]).locale,
            Some(i18n::Locale::En)
        );
        assert_eq!(cli(&["/tmp/x"]).locale, None);
        // `--lang` 后面没跟语言：不是路径，别把 `--lang` 自己当成语言，
        // 也别顺手把下一个参数吞成语言。
        assert_eq!(cli(&["--lang"]).locale, None);
    }

    #[test]
    fn only_the_first_path_counts() {
        // 一次启动只开一个地方；多给的忽略，不做"每个参数开一窗"。
        let launch = cli(&["/tmp/a", "/tmp/b"]).launch.unwrap();
        assert_eq!(launch.target, Target::Open(PathBuf::from("/tmp/a")));
    }

    #[test]
    fn new_tab_without_a_path_means_nothing() {
        assert!(cli(&["--new-tab"]).launch.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_still_a_path() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
        // 铁律：路径一律按字节走。认不出 UTF-8 的参数只可能是路径
        // （开关都是 ASCII），不能顺手把它当垃圾扔了。
        let raw = OsString::from_vec(b"/tmp/\xff\xfe".to_vec());
        let launch = parse([raw]).launch.expect("非 UTF-8 的路径也是路径");
        assert_eq!(
            launch.target.path().as_os_str().as_bytes(),
            b"/tmp/\xff\xfe"
        );
    }
}
