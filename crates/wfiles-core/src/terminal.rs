//! 终端集成：在当前目录打开一个终端窗口。
//!
//! 三件事分开，各自可测：
//! - [`candidates`] 列出本平台能用的终端（`cfg` 在编译期就过滤掉别的平台）；
//! - [`command`] 把目录填进参数模板，产出 `std::process::Command`，不走 shell；
//! - [`installed`] 判断某个终端在不在（macOS 看 .app，其它平台扫 PATH）。
//!
//! 真正拉起进程只在 [`open`] 一处，测试只碰前面三个，不会真的弹出窗口。

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// 让用户选"自动"：由 [`resolve`] 挑一个本机装着的（Linux 上先按桌面环境挑）。
pub const AUTO: &str = "auto";

/// 一个可用的终端程序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terminal {
    /// 设置里存的标识，稳定不变（改显示名不影响用户的选择）。
    pub id: &'static str,
    /// 菜单里显示的名字。
    pub name: &'static str,
    /// 要启动的程序。
    pub program: &'static str,
    /// 判断"装没装"该看谁。
    ///
    /// 多数终端就是 `program`，但有两处不是：Windows 上那几条走
    /// `cmd /c start`，`program` 是 cmd（永远都在），真正要问的是后面拉起的
    /// 那个 exe；macOS 上则是 .app 的名字（不含 `.app`）——启动用的是
    /// `open -a <同一个名字>`，两边必须一致。
    pub probe: &'static str,
    /// 参数模板，`{dir}` 会被换成目标目录的绝对路径。
    pub args: &'static [&'static str],
}

impl Terminal {
    /// 挑出第一个装了的候选。
    pub fn pick(list: &[Terminal]) -> Option<Terminal> {
        list.iter().copied().find(installed)
    }
}

/// 本平台可用的终端，按推荐顺序排。
///
/// 顺序只在 [`auto`] 里起作用（第一个装了的胜出）。Terminal.app 永远在
/// 列表最前面：它在 macOS 上是系统自带的，`auto` 因此总能有个确定答案。
///
/// 几个现代终端（Ghostty / WezTerm / kitty / Alacritty）是命令行优先的，
/// macOS 上得把目录当**启动参数**递进去（`open -a <App> --args …`）——
/// 它们不像 Terminal.app 那样认"打开这个目录"的文档请求。代价是**已经
/// 开着的实例可能不认**这个参数（`open` 只在启动那一刻传参），这一点
/// 在 terminal.rs 里没有别的办法绕，只能记在这儿。
pub fn candidates() -> Vec<Terminal> {
    #[cfg(target_os = "macos")]
    {
        vec![
            Terminal {
                id: "terminal",
                name: "终端（Terminal.app）",
                program: "open",
                probe: "Terminal",
                args: &["-a", "Terminal", "{dir}"],
            },
            Terminal {
                id: "iterm",
                name: "iTerm2",
                program: "open",
                probe: "iTerm",
                args: &["-a", "iTerm", "{dir}"],
            },
            Terminal {
                id: "warp",
                name: "Warp",
                program: "open",
                probe: "Warp",
                args: &["-a", "Warp", "{dir}"],
            },
            Terminal {
                id: "ghostty",
                name: "Ghostty",
                program: "open",
                probe: "Ghostty",
                args: &["-a", "Ghostty", "--args", "--working-directory={dir}"],
            },
            Terminal {
                id: "wezterm",
                name: "WezTerm",
                program: "open",
                probe: "WezTerm",
                args: &["-a", "WezTerm", "--args", "start", "--cwd", "{dir}"],
            },
            Terminal {
                id: "kitty",
                name: "kitty",
                program: "open",
                probe: "kitty",
                args: &["-a", "kitty", "--args", "--directory", "{dir}"],
            },
            Terminal {
                id: "alacritty",
                name: "Alacritty",
                program: "open",
                probe: "Alacritty",
                args: &["-a", "Alacritty", "--args", "--working-directory", "{dir}"],
            },
        ]
    }

    #[cfg(target_os = "windows")]
    {
        vec![
            Terminal {
                id: "wt",
                name: "Windows Terminal",
                program: "cmd",
                probe: "wt.exe",
                // start 的第一个带引号参数是窗口标题，必须有，
                // 否则 start 会把后面引号里的路径当成标题。
                args: &["/c", "start", "WFiles", "wt.exe", "-d", "{dir}"],
            },
            Terminal {
                id: "powershell",
                name: "PowerShell",
                program: "cmd",
                probe: "powershell.exe",
                args: &["/c", "start", "WFiles", "/D", "{dir}", "powershell.exe"],
            },
            Terminal {
                id: "cmd",
                name: "命令提示符",
                program: "cmd",
                probe: "cmd.exe",
                args: &["/c", "start", "WFiles", "/D", "{dir}", "cmd.exe"],
            },
            // 下面几个是各自发行的 Windows 版：装了就列出来，没装的自然被
            // `installed` 滤掉。
            Terminal {
                id: "ghostty",
                name: "Ghostty",
                program: "cmd",
                probe: "ghostty.exe",
                args: &[
                    "/c",
                    "start",
                    "WFiles",
                    "/D",
                    "{dir}",
                    "ghostty.exe",
                    "--working-directory={dir}",
                ],
            },
            Terminal {
                id: "wezterm",
                name: "WezTerm",
                program: "cmd",
                probe: "wezterm.exe",
                args: &[
                    "/c",
                    "start",
                    "WFiles",
                    "/D",
                    "{dir}",
                    "wezterm.exe",
                    "start",
                    "--cwd",
                    "{dir}",
                ],
            },
            Terminal {
                id: "kitty",
                name: "kitty",
                program: "cmd",
                probe: "kitty.exe",
                args: &[
                    "/c",
                    "start",
                    "WFiles",
                    "/D",
                    "{dir}",
                    "kitty.exe",
                    "--directory",
                    "{dir}",
                ],
            },
            Terminal {
                id: "alacritty",
                name: "Alacritty",
                program: "cmd",
                probe: "alacritty.exe",
                args: &[
                    "/c",
                    "start",
                    "WFiles",
                    "/D",
                    "{dir}",
                    "alacritty.exe",
                    "--working-directory",
                    "{dir}",
                ],
            },
        ]
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            Terminal {
                id: "gnome-terminal",
                name: "GNOME 终端",
                program: "gnome-terminal",
                probe: "gnome-terminal",
                args: &["--working-directory={dir}"],
            },
            Terminal {
                id: "konsole",
                name: "Konsole",
                program: "konsole",
                probe: "konsole",
                args: &["--workdir", "{dir}"],
            },
            Terminal {
                id: "xfce4-terminal",
                name: "XFCE 终端",
                program: "xfce4-terminal",
                probe: "xfce4-terminal",
                args: &["--working-directory={dir}"],
            },
            Terminal {
                id: "ghostty",
                name: "Ghostty",
                program: "ghostty",
                probe: "ghostty",
                args: &["--working-directory={dir}"],
            },
            Terminal {
                id: "wezterm",
                name: "WezTerm",
                program: "wezterm",
                probe: "wezterm",
                args: &["start", "--cwd", "{dir}"],
            },
            Terminal {
                id: "alacritty",
                name: "Alacritty",
                program: "alacritty",
                probe: "alacritty",
                args: &["--working-directory", "{dir}"],
            },
            Terminal {
                id: "kitty",
                name: "kitty",
                program: "kitty",
                probe: "kitty",
                args: &["--directory", "{dir}"],
            },
            Terminal {
                id: "x-terminal-emulator",
                name: "系统默认终端",
                program: "x-terminal-emulator",
                probe: "x-terminal-emulator",
                args: &["--working-directory={dir}"],
            },
        ]
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        Vec::new()
    }
}

/// 按标识取终端；`auto` 交给 [`resolve`] 自动挑。
pub fn by_id(id: &str) -> Option<Terminal> {
    candidates().into_iter().find(|terminal| terminal.id == id)
}

/// 解析用户的选择：`auto` 挑第一个装了的，显式 id 找不到就退回自动。
///
/// 找不到任何终端时返回 `None`（例如 Linux 上只装了没列进来的冷门终端），
/// 由调用方决定怎么告诉用户。
pub fn resolve(id: &str) -> Option<Terminal> {
    if id != AUTO
        && let Some(terminal) = by_id(id)
    {
        return Some(terminal);
    }
    auto()
}

/// 自动选择：桌面环境对得上的优先，否则取第一个装了的。
fn auto() -> Option<Terminal> {
    let list = candidates();

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // 跟着桌面环境走，比“列表里第一个装了什么”更贴合用户预期。
        for id in desktop_candidates() {
            if let Some(terminal) = by_id(id)
                && installed(&terminal)
            {
                return Some(terminal);
            }
        }
    }

    Terminal::pick(&list)
}

/// 按桌面环境排的候选顺序（只读环境变量，方便测试）。
#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_candidates() -> Vec<&'static str> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_lowercase();

    if desktop.contains("kde") || desktop.contains("plasma") {
        return vec!["konsole"];
    }
    if desktop.contains("xfce") {
        return vec!["xfce4-terminal"];
    }
    if desktop.contains("gnome") || desktop.contains("ubuntu") || desktop.contains("pop") {
        return vec!["gnome-terminal", "x-terminal-emulator"];
    }
    Vec::new()
}

/// 这个终端装了没有。
pub fn installed(terminal: &Terminal) -> bool {
    #[cfg(target_os = "macos")]
    {
        // macOS 上真正启动的是 `open -a <名字>`，`probe` 就是那个 .app 的
        // 名字（不含 `.app`）。两个名字不一致的话，用户会看到一个装着的
        // 终端被判成没装，或者反过来点下去报错——`macos_probe_and_launch_
        // name_agree` 就是钉这一条的。
        mac_app_exists(terminal.probe)
    }

    #[cfg(not(target_os = "macos"))]
    {
        find_program(terminal.probe).is_some()
    }
}

/// macOS：在 /Applications 与 ~/Applications（含“实用工具”子目录）里找 .app。
#[cfg(target_os = "macos")]
fn mac_app_exists(name: &str) -> bool {
    let mut roots: Vec<PathBuf> = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/Applications/Utilities"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }
    let app = format!("{name}.app");
    roots
        .iter()
        .any(|root| root.join(&app).exists() || root.join(name).join(&app).exists())
}

/// 在 PATH 外加常见安装位置里找可执行程序；找到返回完整路径。
///
/// GUI 应用拿到的 PATH 往往比终端里短（macOS 上尤其如此），
/// 所以 Homebrew、Windows 的应用执行别名目录要另外补。
///
/// macOS 上 `installed` 只看 .app，这个函数就只有单测在用。
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn find_program(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();

    #[cfg(target_os = "macos")]
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);

    #[cfg(all(unix, not(target_os = "macos")))]
    dirs.extend([
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/snap/bin"),
        PathBuf::from("/var/lib/flatpak/exports/bin"),
    ]);

    // Windows Terminal 是应用执行别名，不在 PATH 目录里，只在 WindowsApps 下。
    #[cfg(target_os = "windows")]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft").join("WindowsApps"));
    }

    // 带目录分隔符就按原样查，不去 PATH 里拼。
    if name.contains('/') || name.contains('\\') {
        let path = PathBuf::from(name);
        return path.exists().then_some(path);
    }

    let with_exe = if cfg!(target_os = "windows") && !name.ends_with(".exe") {
        Some(format!("{name}.exe"))
    } else {
        None
    };

    dirs.iter().find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        let candidate = dir.join(with_exe.as_deref()?);
        candidate.exists().then_some(candidate)
    })
}

/// 把参数模板里的 `{dir}` 换成目录。
///
/// 用 `OsString` 而不是 `String`：路径不保证是合法 UTF-8，
/// `to_string_lossy` 会把非 UTF-8 路径改坏，子进程就找不着目录了。
fn expand(template: &str, dir: &Path) -> OsString {
    const MARKER: &str = "{dir}";

    let mut out = OsString::new();
    let mut rest = template;
    while let Some(index) = rest.find(MARKER) {
        out.push(&rest[..index]);
        out.push(dir);
        rest = &rest[index + MARKER.len()..];
    }
    out.push(rest);
    out
}

/// 构造“在 `dir` 打开终端”的命令。调用方负责 spawn。
pub fn command(terminal: &Terminal, dir: &Path) -> Command {
    let mut command = Command::new(terminal.program);
    command.args(terminal.args.iter().map(|arg| expand(arg, dir)));
    // 子进程继承我们的工作目录没有意义，显式给目标目录，
    // 这样那些不认 --working-directory 的终端也至少起点正确。
    command.current_dir(dir);
    command
}

/// 打开一个终端窗口。返回实际用的是哪个终端，失败时给人能看懂的原因。
pub fn open(id: &str, dir: &Path) -> Result<String, crate::i18n::Msg> {
    let Some(terminal) = resolve(id) else {
        return Err(crate::i18n::Message::msg("没找到可用的终端程序"));
    };
    if !dir.is_dir() {
        return Err(crate::i18n::Message::args(
            "{path} 不是目录",
            &[("path", dir.display().to_string())],
        ));
    }

    command(&terminal, dir)
        .spawn()
        .map(|_| terminal.name.to_string())
        .map_err(|error| {
            crate::i18n::Message::args(
                "启动{name}失败：{error}",
                &[
                    ("name", terminal.name.to_string()),
                    ("error", error.to_string()),
                ],
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Terminal {
        Terminal {
            id: "sample",
            name: "样例终端",
            program: "sample-term",
            probe: "sample-term",
            args: &["--cwd", "{dir}", "--working-directory={dir}"],
        }
    }

    #[test]
    fn expand_keeps_the_path_intact() {
        let arg = expand("--working-directory={dir}", Path::new("/tmp/a b"));
        assert_eq!(arg, OsString::from("--working-directory=/tmp/a b"));
    }

    #[test]
    fn expand_leaves_templates_without_the_marker_alone() {
        assert_eq!(expand("start", Path::new("/tmp")), OsString::from("start"));
    }

    #[test]
    fn command_puts_the_directory_where_the_template_says() {
        let command = command(&sample(), Path::new("/work"));
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(program, "sample-term");
        assert_eq!(args, vec!["--cwd", "/work", "--working-directory=/work"]);
    }

    #[test]
    fn candidate_ids_are_unique() {
        let list = candidates();
        let mut ids: Vec<&str> = list.iter().map(|terminal| terminal.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "终端 id 撞车会让设置存错对象");
    }

    #[test]
    fn every_candidate_uses_the_directory_placeholder() {
        for terminal in candidates() {
            assert!(
                terminal.args.iter().any(|arg| arg.contains("{dir}")),
                "{} 的参数里没有 {{dir}}，会打开在别的目录",
                terminal.id
            );
        }
    }

    /// 四个现代终端（Ghostty / WezTerm / kitty / Alacritty）**三个平台上都得
    /// 有**：用户装的是哪一个不由我们决定，而我们只发一份候选表。少一个就是
    /// "Linux 里能选、Windows 里选不到"这种没法跟用户解释的差别。
    #[test]
    fn the_four_modern_terminals_are_listed_everywhere() {
        let ids: Vec<&str> = candidates().iter().map(|terminal| terminal.id).collect();
        for id in ["ghostty", "wezterm", "kitty", "alacritty"] {
            assert!(ids.contains(&id), "{id} 不在候选表里：{ids:?}");
        }
    }

    /// 每个候选都得说清"看谁在不在"。`probe` 写错的后果是它被判成永远没装，
    /// 或者永远装着——Windows 上那几条曾经就是后者：`program` 是 `cmd`，
    /// 而 cmd 永远都在，于是"没装的 Ghostty"也会列出来，点下去只能得到
    /// 一句 cmd 的报错。
    #[test]
    fn every_candidate_declares_what_installed_means() {
        for terminal in candidates() {
            assert!(!terminal.probe.is_empty(), "{} 没写 probe", terminal.id);
        }
    }

    /// macOS 上"看谁在不在"（`probe`，拼成 `<名字>.app`）和"拉起谁"
    /// （`open -a <名字>`）必须是同一个名字：不一致时，用户会看到一个装着
    /// 的终端被判成没装；反过来则是点下去报错。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_probe_and_launch_name_agree() {
        for terminal in candidates() {
            let launched = terminal
                .args
                .iter()
                .position(|arg| *arg == "-a")
                .and_then(|index| terminal.args.get(index + 1))
                .copied()
                .expect("macOS 上每个候选都靠 `open -a` 启动");
            assert_eq!(
                terminal.probe, launched,
                "{} 的 probe 与 -a 的名字对不上",
                terminal.id
            );
        }
    }

    #[test]
    fn explicit_id_wins_over_auto() {
        let list = candidates();
        let last = *list.last().expect("本平台至少有一个候选");
        assert_eq!(by_id(last.id), Some(last));
        assert_eq!(resolve(last.id), Some(last));
    }

    #[test]
    fn unknown_id_falls_back_to_auto() {
        // 解析不到就退回自动，绝不因为一个脏设置把功能整个废掉。
        let resolved = resolve("这种终端不存在");
        let expected = resolve(AUTO);
        assert_eq!(resolved.map(|it| it.id), expected.map(|it| it.id));
    }

    #[test]
    fn auto_picks_an_installed_terminal() {
        // 开发环境里至少得装着一个终端；装了 N 个时取第一个装了的。
        if let Some(terminal) = resolve(AUTO) {
            assert!(installed(&terminal), "自动选择不能挑一个没装的");
        }
    }

    #[test]
    fn opening_in_a_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let error = open(AUTO, &file).unwrap_err();
        assert!(error.to_string().contains("不是目录"), "实际错误：{error}");
    }

    #[cfg(unix)]
    #[test]
    fn find_program_locates_a_standard_binary() {
        // sh 在所有 Unix 上都有，用来验证 PATH 扫描本身是通的。
        assert!(find_program("sh").is_some());
        assert!(find_program("肯定没有这个程序").is_none());
    }

    #[test]
    fn the_resolved_command_really_targets_the_directory() {
        // 整条链路的收口检查：从设置里的 id 一路走到真正要执行的命令行。
        // 一个终端都没装的机器（比如没装 X 的 Linux 容器）上跳过，
        // 别让 CI 因为环境而不是因为代码变红。
        let Some(terminal) = resolve(AUTO) else {
            return;
        };
        let dir = Path::new("/tmp");
        let command = command(&terminal, dir);
        assert!(
            !command.get_program().is_empty(),
            "{} 没有可执行程序",
            terminal.id
        );
        assert_eq!(
            command.get_current_dir(),
            Some(dir),
            "cwd 没设成目标目录，终端会开在进程自己的目录里"
        );
    }
}
