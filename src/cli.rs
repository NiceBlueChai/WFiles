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
//! 但"**两句话互相矛盾**"要报出来：`--new-window` 与 `--new-tab` 一起给，
//! 猜哪个都是替用户做决定——而两种猜法都长得像"照办了"。这一种不开窗，
//! 退出码 2，用法由 `main` 打印（见 [`Cli::problem`]）。
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
    /// `--new-window`：自己开一扇新窗，别交给已经开着的那一扇。
    ///
    /// 别的启动都是"有话交给已经开着的那一扇去办"（见 `instance`），
    /// 只有它明说自己开：脚本要的是"给我一扇看着这个目录的窗"，
    /// 而不是"把我正看着的那扇挪走"。
    ///
    /// **只有已经开着一份时它才另有意思**：没有别的窗时，这一扇就是主窗，
    /// 会话、布局照常归它（见 `app::Starting`）。
    pub new_window: bool,
    /// 命令行自己打架时的一句话（i18n key）。有值就别开窗。
    pub problem: Option<&'static str>,
}

/// 用错了命令行时打印的第一行之后的用法说明。走 i18n，见 [`Cli::problem`]。
pub const USAGE: &str = "用法：wfiles [路径] [--reveal] [--new-tab | --new-window] [--lang=代码]";

/// `--new-window` 与 `--new-tab` 一起给：两句话互相矛盾。
///
/// 新开的那一扇窗里只有一栏——没有"另一栏"可以加。往哪边猜都不对。
const BOTH_FLAGS: &str =
    "--new-window 与 --new-tab 不能一起用：新开的那一扇窗里只有一栏，没有「另一栏」可加";

/// 启动目标，以及开法。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub target: Target,
    /// `--new-tab`：另开一个标签页，而不是把当前那一栏挪过去。
    pub new_tab: bool,
}

/// 认识的就这么几个：`--lang`（`=` 与空格两种写法）、`--reveal`、
/// `--new-tab`、`--new-window`，以及一条路径。别的放过。
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
        } else if text == "--new-window" {
            cli.new_window = true;
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
    // 两个开关要的都是"这一栏"：一个说另开一栏、一个说另开一窗。
    // 没给路径时 `--new-tab` 本来就什么都不表示（上面那句），
    // 于是也没真的打架——只有带着路径时才是在说两件相反的事。
    if cli.new_window && cli.launch.as_ref().is_some_and(|launch| launch.new_tab) {
        cli.problem = Some(BOTH_FLAGS);
    }
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

    #[test]
    fn new_window_is_remembered_and_does_not_disturb_the_path() {
        assert!(cli(&["--new-window"]).new_window);
        assert!(!cli(&["/tmp/x"]).new_window);
        // 位置随便：开关在路径前面后面都一样。
        for args in [
            vec!["--new-window", "/tmp/x"],
            vec!["/tmp/x", "--new-window"],
        ] {
            let cli = cli(&args);
            assert!(cli.new_window);
            assert!(cli.problem.is_none());
            let launch = cli.launch.expect("路径照样认得");
            assert_eq!(launch.target, Target::Open(PathBuf::from("/tmp/x")));
            assert!(!launch.new_tab, "另开一窗不是另开一栏");
        }
    }

    #[test]
    fn new_window_and_new_tab_together_is_a_usage_error() {
        // 两句话互相矛盾：新窗里只有一栏，没有"另一栏"可加。
        // 猜哪个都是替用户做决定，而两种猜法看起来都像"照办了"。
        let cli = cli(&["--new-window", "--new-tab", "/tmp/x"]);
        assert!(cli.problem.is_some(), "该报出来，而不是挑一个照办");
        assert!(cli.launch.is_some(), "话是听懂了，只是两句话打架");
    }

    #[test]
    fn new_tab_without_a_path_is_not_a_conflict() {
        // `--new-tab` 没给路径时本来就什么都不表示（上面那条测试），
        // 于是它跟 `--new-window` 没真的打架：那一扇窗照开。
        let cli = cli(&["--new-window", "--new-tab"]);
        assert!(cli.problem.is_none());
        assert!(cli.new_window);
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
