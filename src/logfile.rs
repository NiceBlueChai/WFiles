//! 本地日志：后台任务出问题时留给用户交给我们排查的现场。
//!
//! **为什么有它**：状态栏上那句「正在拷贝 X…」一关窗口就没了。后台搬运、
//! 后台索引、远程连接失败这些事，界面上只留最后一句结果，用户没法把
//! 现场交出来——于是每次反馈都是"我这儿刚才好像卡了一下"，谁也定位不了。
//!
//! **日志文案不翻译**，跟 i18n 那套分层的判断是同一条：
//!
//! | 层 | 翻不翻 | 为什么 |
//! |---|---|---|
//! | 界面文案 | 翻 | 用户每天看 |
//! | 错误信息（我们的措辞） | 翻 | 上屏给用户看 |
//! | **日志** | **不翻** | 给排查看的。排查看的人问的是"当时发生了什么"，日志里混着服务器的原始报文、IO 错误串、第三方库的措辞 —— 一句没翻过的英文 `ENOENT: no such file` 比翻成中文更好认 |
//!
//! 这也是为什么 [`log`] 这个函数收的是 `&str` 而不是
//! [`crate::i18n::Msg`]：**没有语言可翻这件事，要写进类型里**，
//! 不然将来有人顺手写 `log(&msg)` 就把它混进翻译路径了。
//!
//! 三条取舍：
//!
//! - **自己写，不接 GPUI 自带的 `gpui-pre-zlog`。** 它的
//!   `init_output_file` 把路径存 `OnceLock` 且要求 `&'static PathBuf` ——
//!   一个进程只能设一次，测试就没法传临时目录（项目铁律是所有测试必须
//!   走临时目录，绝不写真机缓存）。而日志这件事本身只需要"追加一行、
//!   超过上限就轮转"这么点逻辑。
//! - **写在缓存目录，不写在状态目录。** 日志是"删了也不心疼"的东西，
//!   跟 [`crate::resume`] 的暂存同一套定位规则。
//! - **失败静默。** 写不进去不能让程序崩——日志是给人排查看的东西，
//!   而它自己在排查看的路径上崩了，是最坏的一种崩。

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// 超过这个大小就把现有内容挪成 `.1`，从新文件重新写。
///
/// 1 MB 是"够看完一整页"的量级：一次排查要看的通常就是最近那几十次
/// 操作，行均几十字节，几百行足够。多留的历史在这个用途里没有意义，
/// 而日志文件是**每次启动都在写**的东西，不设上限的话它就是那类
/// "用户从不敢删、于是越来越大"的文件。
const MAX_BYTES: u64 = 1024 * 1024;

/// 一行日志的级别。抄 `log` crate 的那套名字，省得自己发明一套。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 出错了：搬运失败、连不上、写不进盘。
    Error,
    /// 不对劲但还能用：一次传输被中止、索引扫到上限。
    Warn,
    /// 关键节点：一次搬运的起止、一轮索引的统计。
    Info,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 记一行。
///
/// 收 `&str` 不收 [`crate::i18n::Msg`]：这个文件里的句子**不翻译**，
/// 理由写在模块头里。类型上就把它跟翻译路径断开，比写注释管用。
pub fn log(level: Level, message: &str) {
    // 日志文件建不出来就什么都不记。这不是可以吞掉的错误：
    // 能报的地方正是那个不存在的文件。
    let Some(_path) = log_path() else { return };
    let line = format!("{} {level} {message}\n", stamp());
    // 锁只护"追加 + 落盘"这一小段。日志写入在界面线程上，
    // 而搬运线程也会写——不锁的话两行可能交错成半行，
    // 而排查看的现场里出现一行破碎的字节，比少一行糟糕得多。
    let Ok(mut guard) = SINK.lock() else {
        // 上一次写的时候 panicked（锁被投毒）。日志不该把程序带下去，
        // 放弃这一行，下一行接着写。
        return;
    };
    let Some(sink) = guard.as_mut() else { return };
    let written = sink.write_all(line.as_bytes()).and_then(|()| sink.flush());
    if written.is_err() {
        // 磁盘满了、日志文件被手工删了、缓存目录被清理工具动过。
        // 丢掉句柄让下一次 [`log`] 重新打开——持有句柄的话一条也写不进去，
        // 而"下一条就好"比"这一整个进程都不再记录"值得争取。
        *guard = None;
    }
}

/// [`Level::Info`] 的简写。
pub fn info(message: &str) {
    log(Level::Info, message);
}

/// [`Level::Warn`] 的简写。
pub fn warn(message: &str) {
    log(Level::Warn, message);
}

/// [`Level::Error`] 的简写。
pub fn error(message: &str) {
    log(Level::Error, message);
}

/// 日志文件在哪儿。没落盘的路径返回 `None`（第一次 [`log`] 之前）。
pub fn path() -> Option<PathBuf> {
    LOG_PATH.get().cloned()
}

/// 把日志复制一份到 `dest`，返回真正写出来的路径。
///
/// 这是给用户的那一下：日志文件在缓存目录里，用户不知道也不该知道
/// 在哪儿；「导出日志」把他要交出去的那份放到看得见的地方。
pub fn export_to(dest: &Path) -> std::io::Result<PathBuf> {
    // 先把缓冲里的尾巴落盘，否则导出的那份比屏幕上看到的那份少几行。
    // 用户刚看到一句失败就来导出，少的那几行往往正是他要的。
    flush();
    let Some(source) = log_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "日志文件还没有建立",
        ));
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // 目标跟源是同一个路径就直接返回：用户点了「导出日志」而导出目的地
    // 恰好就是当前日志文件时，复制会把文件截断成空的（先开目标写、再读源）。
    let same = fs::canonicalize(&source).ok() == fs::canonicalize(dest).ok();
    if !same {
        fs::copy(&source, dest)?;
    }
    Ok(dest.to_path_buf())
}

/// 把缓冲里的尾巴落盘。
///
/// 导出前调它：否则用户看到的和导出的差几行，而少的那几行
/// 往往正是他跑来导出的原因。
pub fn flush() {
    let Ok(mut guard) = SINK.lock() else { return };
    if let Some(sink) = guard.as_mut() {
        let _ = sink.flush();
    }
}

// ---- 落盘 ----

/// 已打开的日志文件。`None` = 现在没有可写的句柄（写失败过，等下次重开）。
static SINK: Mutex<Option<fs::File>> = Mutex::new(None);
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 第一次 [`log`] 时把文件打开，顺手定下路径。
///
/// 路径存起来是为了 [`export_to`]：导出那份的时候得知道源在哪儿。
/// 用 `OnceLock` 是因为**一次运行只有一个日志文件**——这个约束不是
/// "省事"，是语义：用户导出的和程序在写的是同一份现场。
fn log_path() -> Option<PathBuf> {
    if let Some(path) = LOG_PATH.get() {
        return Some(path.clone());
    }
    let dir = root();
    // 建目录失败就彻底不记了：没有目录的日志文件写不出来，
    // 而把错误报到哪里去——正好是那个不存在的日志文件。
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("wfiles.log");
    let Ok(mut guard) = SINK.lock() else {
        return None;
    };
    let file = open_log_file(&path).ok()?;
    *guard = Some(file);
    let _ = LOG_PATH.set(path.clone());
    Some(path)
}

/// 打开日志文件，顺手做轮转。
///
/// 轮转在**打开时**做而不是每次追加时做：这个文件整个进程生命周期
/// 基本只开一次（写坏了会重开，那时也走这里），每次追加都去 `metadata`
/// 查长度等于给每次写一行加一次系统调用。
fn open_log_file(path: &Path) -> std::io::Result<fs::File> {
    open_log_file_up_to(path, MAX_BYTES)
}

/// 打开日志文件（超过 `max_bytes` 就先轮转）。
///
/// 上限是参数而不是常量直读：轮转这条判据写「造一个超限的文件再打开」
/// 就得真的写 [`MAX_BYTES`]（1 MB）才触发——一次测试几百 MB 的 IO，
/// 而**判据应该比实现便宜**。传 8 字节进去，同样的代码路径，
/// 代价是一次 `write_all`。
fn open_log_file_up_to(path: &Path, max_bytes: u64) -> std::io::Result<fs::File> {
    // 先看要不要轮转。已存在且超限 → 挪成 `.1`。
    let too_big = fs::metadata(path)
        .map(|meta| meta.len() >= max_bytes)
        .unwrap_or(false);
    if too_big {
        let mut older = path.as_os_str().to_owned();
        older.push(".1");
        // 挪不动就当没有：最坏结果是文件超了上限继续涨，
        // 而为一个日志文件去 `remove_file` 用户的东西（万一 `.1` 是目录）
        // 是本末倒置。
        let _ = fs::rename(path, older);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// 时间戳：`HH:MM:SS.mmm`。
///
/// **不带日期**：这个日志文件一个进程生命周期里最多跨一次午夜，
/// 而跨了之后下一次启动就会轮转（先看长度、再看日期都没意义）。
/// 少了日期就少一列 10 个字符，而一屏能多看十几行现场。
fn stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs();
    let millis = now.subsec_millis();
    let minute_of_day = seconds.rem_euclid(86_400) / 60;
    format!(
        "{:02}:{:02}:{:02}.{millis:03}",
        minute_of_day / 60,
        minute_of_day % 60,
        seconds.rem_euclid(60)
    )
}

/// 日志目录：跟 [`crate::resume`] 的暂存同一套定位规则。
///
/// 测试构建下固定走系统临时目录：真机的日志写在 `~/Library/Caches/WFiles/logs`，
/// 而测试里遍地是"故意让它失败"的用例，不隔开的话每跑一轮 `cargo test`
/// 都往用户的缓存目录里灌一批故障现场。
pub fn root() -> PathBuf {
    #[cfg(test)]
    {
        super::resume::test_root().join("logs")
    }
    #[cfg(not(test))]
    {
        match crate::session::session_dir_override() {
            Some(dir) => dir.join("logs"),
            None => dirs::cache_dir()
                .or_else(dirs::state_dir)
                .unwrap_or_else(|| std::env::temp_dir().join("wfiles"))
                .join("WFiles")
                .join("logs"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程里只有一个日志文件，所以这些用例只能互相配合着跑，
    /// 不能各自传目录进来。这把锁护的就是那件事。
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_line_really_lands_in_the_file() {
        let _guard = guard();
        log(Level::Info, "搬运开始");
        flush();
        let path = path().expect("第一次 log 就把路径定下来了");
        let text = fs::read_to_string(&path).expect("日志文件读得回来");
        assert!(
            text.contains("搬运开始"),
            "文件里该有刚写的那一句，实际内容：{text:?}"
        );
    }

    #[test]
    fn the_level_shows_up_on_the_line() {
        let _guard = guard();
        error("连接被拒");
        flush();
        let text = fs::read_to_string(path().expect("有路径")).expect("读得回来");
        assert!(text.contains("ERROR"), "级别该在行里，实际：{text:?}");
    }

    #[test]
    fn a_line_carries_a_readable_clock() {
        let _guard = guard();
        info("记一笔");
        flush();
        let text = fs::read_to_string(path().expect("有路径")).expect("读得回来");
        let last = text.lines().next_back().expect("至少有一行");
        let clock = last.split(' ').next().expect("行首是时间戳");
        let parts: Vec<&str> = clock.split(':').collect();
        assert_eq!(parts.len(), 3, "时间戳该是 时:分:秒.毫秒，实际 {clock:?}");
        let hour: u32 = parts[0].parse().expect("小时是数字");
        assert!(hour < 24, "小时该在 0..24，实际 {hour}");
        let minute: u32 = parts[1].parse().expect("分是数字");
        assert!(minute < 60, "分该在 0..60，实际 {minute}");
    }

    #[test]
    fn exporting_puts_a_copy_where_the_user_can_see_it() {
        let _guard = guard();
        info("导出前记一笔");
        let out = tempfile::tempdir().expect("临时目录");
        let landed = export_to(&out.path().join("wfiles-log.txt")).expect("导出该成功");
        let text = fs::read_to_string(&landed).expect("导出的那份读得回来");
        assert!(
            text.contains("导出前记一笔"),
            "刚写的那一句该在导出里，实际：{text:?}"
        );
    }

    #[test]
    fn exporting_onto_the_log_itself_does_not_truncate_it() {
        let _guard = guard();
        info("别把我自己清空了");
        // 目标就是日志文件本身：复制会先开目标写、再读源，
        // 不判一下的话导出的文件是空的（而"导出后日志没了"这种症状
        // 极难定位——它看起来像日志功能坏了）。
        let source = path().expect("有路径");
        let landed = export_to(&source).expect("导出到源自己也算成功");
        let text = fs::read_to_string(&landed).expect("读得回来");
        assert!(
            text.contains("别把我自己清空了"),
            "导出到源自己时该原样留着，实际长度 {}",
            text.len()
        );
    }

    #[test]
    fn the_log_lives_in_a_temporary_directory_while_testing() {
        // 这条不是"测功能"，是**守住隔离**：日志文件每次运行都在写，
        // 一旦哪次改动让它落到真机缓存目录，跑一轮测试就给用户的
        // 缓存里灌一批故障现场，而这类污染不会让任何测试变红。
        let root = root();
        assert!(
            root.starts_with(std::env::temp_dir()),
            "测试构建的日志目录该在系统临时目录下，实际 {root:?}"
        );
    }

    #[test]
    fn a_full_log_moves_aside_instead_of_growing_forever() {
        let _guard = guard();
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("wfiles.log");
        fs::write(&path, "0123456789").expect("先写一个超过上限的文件");

        // 上限 8 字节，文件 10 字节 → 该挪走。
        let mut handle = open_log_file_up_to(&path, 8).expect("轮转后照样能打开");
        handle.write_all(b"new line\n").expect("新文件该写得进");

        let rotated = dir.path().join("wfiles.log.1");
        assert!(
            rotated.exists(),
            "超限的旧内容该挪成 .1，实际目录内容：{:?}",
            fs::read_dir(dir.path()).map(|it| it
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect::<Vec<_>>())
        );
        assert_eq!(
            fs::read_to_string(&rotated).expect("读旧的"),
            "0123456789",
            "挪走的那份该是原封不动的旧内容"
        );
        let fresh = fs::read_to_string(&path).expect("读新的");
        assert_eq!(fresh, "new line\n", "新文件该只有轮转之后写的");
    }

    #[test]
    fn a_log_under_the_limit_is_kept_and_appended_to() {
        let _guard = guard();
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("wfiles.log");
        fs::write(&path, "old\n").expect("先写一行");

        // 旧内容比上限小 → 不该挪走，接着往后追加。
        let mut handle = open_log_file_up_to(&path, 1024).expect("打开");
        handle.write_all(b"new\n").expect("追加");
        drop(handle);

        assert!(
            !dir.path().join("wfiles.log.1").exists(),
            "没超限就不该有 .1"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("读"),
            "old\nnew\n",
            "该是旧内容 + 新追加"
        );
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_not_a_crash() {
        // 造一个「目录占了日志文件名」的场景：父目录能建，
        // 但 `wfiles.log` 已经是个目录，`OpenOptions::open` 必然失败。
        // 这正是"缓存目录被别的东西占了名"那类真实故障的形状。
        let root = tempfile::tempdir().expect("临时目录");
        let blocked = root.path().join("wfiles.log");
        fs::create_dir_all(&blocked).expect("建得出这个目录");
        let result = fs::OpenOptions::new().append(true).open(&blocked);
        assert!(
            result.is_err(),
            "日志文件名被一个目录占了还不该打不开——夹具没造出场景"
        );
        // 而 [`open_log_file`] 拿到这种路径时返回 Err 而不是 panic。
        // panic 会发生在排查看的路径上，那是最坏的崩法。
        let opened = std::panic::catch_unwind(|| open_log_file(&blocked));
        assert!(
            opened.is_ok() && opened.unwrap().is_err(),
            "打不开就该回 Err，不该 panic"
        );
    }
}
