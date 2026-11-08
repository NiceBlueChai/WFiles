//! 后台搜索：从一个根目录往下走，把 `query` 套在每个文件上。
//!
//! 这一层不认识 UI、不碰 GPUI，只有"走目录、读文件、发结果"三件事，
//! 于是"哪些目录该跳过""多大的文件还值得读""什么时候该停"都能在单测里钉死。
//! 界面那一层（`search_entity.rs`）只负责把结果画出来、把取消按钮接上。
//!
//! 两条硬规矩：
//! - 一次搜索最多收 [`MAX_RESULTS`] 条，再多就停：几万个结果的列表没人看得完，
//!   而且它们会先占满内存。
//! - 内容搜索只读"看起来像文本"的文件，且单个不超过 [`MAX_CONTENT_BYTES`]。

use crate::marks::Marks;
use crate::query::{Query, Record};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// 一次搜索最多收多少条。
pub const MAX_RESULTS: usize = 5_000;

/// 内容搜索时单个文件最多读多少字节。
pub const MAX_CONTENT_BYTES: u64 = 8 * 1024 * 1024;

/// 判"是不是二进制"只看开头这么多字节：够了，而且不用把整个大文件读进来。
const SNIFF_BYTES: usize = 8 * 1024;

/// 攒够这么多条才往回发一批。逐条跨线程发，几万条能把消息队列淹了。
const BATCH: usize = 64;

/// 命中行最多显示这么多个字符。
const LINE_CHARS: usize = 200;

/// 这些目录不进去：它们要么巨大、要么是别的工具的缓存、
/// 要么（`/proc` 那一类）走进去就出不来。
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    "node_modules",
    "bower_components",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "coverage",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".idea",
    ".vscode",
    ".cache",
    ".pnpm-store",
    ".yarn",
    "DerivedData",
    "Pods",
    "Carthage",
    ".terraform",
    "System Volume Information",
    "$RECYCLE.BIN",
];

/// 这些扩展名不读内容。剩下的"未知扩展名"按文本处理，
/// 读进来发现是二进制（见 [`Text::read`]）再丢掉——宁可多读一个，别漏掉
/// `.conf` `.gitignore` 这种没有扩展名的文本。
const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "heic", "avif", "ico", "psd", "mp4",
    "mov", "mkv", "avi", "webm", "m4v", "flv", "wmv", "mpg", "mpeg", "mp3", "wav", "flac", "aac",
    "m4a", "ogg", "opus", "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "dmg", "pdf",
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "exe", "dll", "so", "dylib", "o", "a", "class",
    "jar", "wasm", "db", "sqlite", "sqlite3", "ttf", "otf", "woff", "woff2", "eot",
];

/// 一条命中。
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 内容命中的那一行；只在查询里有 `content:` 时才可能有。
    pub line: Option<Line>,
}

/// 文件里的某一行。
#[derive(Clone, Debug)]
pub struct Line {
    /// 行号，从 1 起。
    pub number: usize,
    /// 那一行的内容（保留原来的大小写，太长会截断）。
    pub text: String,
}

/// 走完一趟的账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 看过多少个条目（目录也算）。
    pub visited: usize,
    /// 发出多少条命中。
    pub matched: usize,
    /// 是不是撞到条数上限提前停了。
    pub truncated: bool,
    /// 是不是被取消了。
    pub cancelled: bool,
}

/// 一次搜索。
#[derive(Clone, Debug)]
pub struct Search {
    pub root: PathBuf,
    pub query: Query,
    /// 最多往下走几层子目录；`None` 是不限，`Some(0)` 是"就根目录这一层"。
    pub max_depth: Option<usize>,
    pub limit: usize,
    /// 打的标签与标的颜色。查询里出现 `tag:` / `color:` 时才用得上，
    /// 但一律带上更省事：不用先去问查询"你要不要"。
    pub marks: Marks,
}

impl Search {
    pub fn new(root: PathBuf, query: Query) -> Self {
        Self {
            root,
            query,
            max_depth: None,
            limit: MAX_RESULTS,
            marks: Marks::default(),
        }
    }

    /// 带上标签与颜色。
    pub fn with_marks(mut self, marks: Marks) -> Self {
        self.marks = marks;
        self
    }

    /// 只往下走 `depth` 层子目录。`0` 是"就这一层，别往里钻"。
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }

    /// 走一遍目录树，命中的逐条交给 `sink`。
    ///
    /// 广度优先：浅处的结果先出来，用户不用等深层走完才看到东西。
    /// `cancel` 被置起来就尽快收手——不会把已经算好的结果丢掉。
    pub fn walk(&self, cancel: &AtomicBool, sink: &mut impl FnMut(Hit)) -> Stats {
        let mut stats = Stats::default();
        // 整趟用一个"现在"：`date:` 是相对这个时刻算的，
        // 每个文件各取一次既慢又会让同一个瞬间出现两种结论。
        let now = SystemTime::now();
        let needles = self.query.content_terms();
        let needs_content = self.query.needs_content();
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        queue.push_back((self.root.clone(), 0));

        while let Some((dir, depth)) = queue.pop_front() {
            if cancel.load(Ordering::Relaxed) {
                stats.cancelled = true;
                return stats;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                // 读不了（没权限、被删了、断开的网络盘）就当这层是空的，
                // 别让一个目录把整趟搜索搞黄。
                continue;
            };
            for entry in entries.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    stats.cancelled = true;
                    return stats;
                }
                if stats.matched >= self.limit {
                    stats.truncated = true;
                    return stats;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                // 符号链接一概不跟：目录环、`/proc` 那种无限深，
                // 都能把搜索拖到天荒地老。
                if crate::entry_kind::is_link_or_junction(&entry) {
                    continue;
                }
                let is_dir = file_type.is_dir();
                if !is_dir && !file_type.is_file() {
                    continue;
                }
                stats.visited += 1;

                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let ext = if is_dir {
                    String::new()
                } else {
                    ext_of(&name).to_lowercase()
                };
                let metadata = entry.metadata().ok();
                let size = metadata.as_ref().map_or(0, |it| it.len());
                let modified = metadata.as_ref().and_then(|it| it.modified().ok());

                let text =
                    if is_dir || !needs_content || binary_ext(&ext) || size > MAX_CONTENT_BYTES {
                        None
                    } else {
                        Text::read(&path)
                    };

                let item = Item {
                    path: &path,
                    name: &name,
                    ext: &ext,
                    is_dir,
                    size,
                    modified,
                    color: self.marks.color_of(&path),
                    tags: self.marks.tags_of(&path),
                    text: text.as_ref(),
                };
                if let Some(hit) = self.consider(&item, now, &needles) {
                    stats.matched += 1;
                    sink(hit);
                }

                if is_dir {
                    // 子目录的深度是 `depth + 1`；还能不能再往下走一层，
                    // 是拿它和上限比（`<` 会让 `Some(1)` 变成"连根目录的子目录都不进"）。
                    let deeper = self.max_depth.is_none_or(|max| depth < max);
                    if deeper && !skipped(&name) {
                        queue.push_back((path, depth + 1));
                    }
                }
            }
        }
        stats
    }

    fn consider(&self, item: &Item, now: SystemTime, needles: &[&str]) -> Option<Hit> {
        let path_text = item.path.to_string_lossy();
        let record = Record {
            name: item.name,
            path: &path_text,
            ext: item.ext,
            is_dir: item.is_dir,
            size: item.size,
            modified: item.modified,
            color: item.color,
            tags: item.tags,
        };
        let text = item.text;
        if !self
            .query
            .matches_at(&record, now, text.map(|it| it.lowered.as_str()))
        {
            return None;
        }
        let line = text.and_then(|it| it.locate(needles));
        Some(Hit {
            path: item.path.to_path_buf(),
            name: item.name.to_string(),
            is_dir: item.is_dir,
            size: item.size,
            modified: item.modified,
            line,
        })
    }
}

/// 一个待判定的条目：`walk` 已经把能拿到的都拿好了，这里只做判定。
struct Item<'a> {
    path: &'a Path,
    name: &'a str,
    ext: &'a str,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    color: Option<&'a str>,
    tags: &'a [String],
    text: Option<&'a Text>,
}

/// 从后台线程跑一趟搜索，结果分批送回。
///
/// 返回的 [`Cancel`] 一被丢掉，搜索就停：界面把上一次的 cancel 换成新的，
/// 旧的那个自然就作废了，不用额外记账。
pub fn spawn(search: Search, sink: Arc<dyn Fn(Event) + Send + Sync>) -> Cancel {
    fn run(search: &Search, cancel: &AtomicBool, sink: &dyn Fn(Event)) {
        let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
        let stats = search.walk(cancel, &mut |hit| {
            batch.push(hit);
            if batch.len() >= BATCH {
                sink(Event::Batch(std::mem::take(&mut batch)));
            }
        });
        if !batch.is_empty() {
            sink(Event::Batch(batch));
        }
        sink(Event::Done(stats));
    }

    let cancel = Cancel::default();
    let flag = cancel.0.clone();
    let sink2 = sink.clone();
    let for_thread = search.clone();
    let started = std::thread::Builder::new()
        .name("wfiles-search".to_string())
        .spawn(move || run(&for_thread, &flag, sink2.as_ref()));
    if started.is_err() {
        // 起不了线程（进程里线程太多）就当场跑完：慢一点，但至少界面不会一直转圈。
        run(&search, &cancel.0, sink.as_ref());
    }
    cancel
}

/// 搜索过程中的两件事：一批结果、或者走完了。
#[derive(Debug)]
pub enum Event {
    Batch(Vec<Hit>),
    Done(Stats),
}

/// 中止一趟搜索的把手。丢掉即中止。
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Drop for Cancel {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// 读进来的一个文件：原文用来显示，小写那份用来比对。
struct Text {
    raw: String,
    lowered: String,
}

impl Text {
    fn read(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        let head = &bytes[..bytes.len().min(SNIFF_BYTES)];
        // 开头这几个 KB 里出现 NUL，几乎可以肯定是二进制。
        if head.contains(&0) {
            return None;
        }
        let raw = String::from_utf8(bytes).ok()?;
        Some(Self {
            lowered: raw.to_lowercase(),
            raw,
        })
    }

    /// 找出第一个含有任一关键词的行。
    ///
    /// 行号从小写那份里数（行数不会因大小写变化），行文字取原文——
    /// 显示出来的那一行得保留用户写的大小写。
    fn locate(&self, needles: &[&str]) -> Option<Line> {
        if needles.is_empty() {
            return None;
        }
        let index = self
            .lowered
            .lines()
            .position(|line| needles.iter().any(|needle| line.contains(needle)))?;
        let text = self.raw.lines().nth(index).unwrap_or_default();
        Some(Line {
            number: index + 1,
            text: truncate(text.trim(), LINE_CHARS),
        })
    }
}

/// 扩展名：`a.tar.gz` 是 `gz`，`.gitignore` 这种点开头的文件没有扩展名。
fn ext_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext,
        _ => "",
    }
}

/// 这个目录要不要跳进去。大小写不敏感：Windows 上大小写本来就不分。
fn skipped(name: &str) -> bool {
    SKIP_DIRS.iter().any(|it| name.eq_ignore_ascii_case(it))
}

fn binary_ext(ext: &str) -> bool {
    !ext.is_empty() && BINARY_EXTS.contains(&ext)
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn tree() -> TempDir {
        let root = TempDir::new().expect("临时目录");
        fs::create_dir_all(root.path().join("src/deep")).expect("建目录");
        write(root.path().join("src/main.rs"), "fn main() {}\n");
        write(root.path().join("src/deep/hidden.rs"), "fn hidden() {}\n");
        write(root.path().join("notes.txt"), "hello world\nsecond line\n");
        write(root.path().join("photo.png"), "hello in a binary-ish file");
        fs::create_dir_all(root.path().join("node_modules/pkg")).expect("建目录");
        write(
            root.path().join("node_modules/pkg/index.js"),
            "hello from a dep",
        );
        root
    }

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("建目录");
        }
        let mut file = fs::File::create(path).expect("建文件");
        file.write_all(contents.as_bytes()).expect("写文件");
    }

    fn found(root: &Path, query: &str) -> Vec<Hit> {
        let search = Search::new(root.to_path_buf(), Query::parse(query).expect("解析"));
        let mut hits = Vec::new();
        search.walk(&AtomicBool::new(false), &mut |hit| hits.push(hit));
        hits
    }

    fn names(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|hit| hit.name.clone()).collect()
    }

    #[test]
    fn a_word_finds_files_anywhere_below_the_root() {
        let root = tree();
        let hits = found(root.path(), "main");
        assert_eq!(names(&hits), vec!["main.rs".to_string()]);
    }

    #[test]
    fn a_wildcard_narrows_by_extension() {
        let root = tree();
        let hits = found(root.path(), "*.rs");
        assert_eq!(names(&hits).len(), 2, "实际：{:?}", names(&hits));
    }

    #[test]
    fn noisy_directories_are_skipped() {
        let root = tree();
        assert!(
            found(root.path(), "hello")
                .iter()
                .all(|hit| hit.name == "notes.txt"),
            "node_modules 里的东西不该出现"
        );
    }

    #[test]
    fn the_depth_limit_stops_the_walk() {
        let root = tree();
        let shallow = Search::new(
            root.path().to_path_buf(),
            Query::parse("*.rs").expect("解析"),
        )
        .with_depth(1);
        let mut hits = Vec::new();
        let stats = shallow.walk(&AtomicBool::new(false), &mut |hit| hits.push(hit));
        assert_eq!(names(&hits), vec!["main.rs".to_string()]);
        assert!(!stats.truncated);
    }

    #[test]
    fn directories_can_match_too() {
        let root = tree();
        let hits = found(root.path(), "type:dir src");
        assert_eq!(names(&hits), vec!["src".to_string()]);
    }

    #[test]
    fn content_search_reports_the_line_it_hit() {
        let root = tree();
        let hits = found(root.path(), "content:second");
        assert_eq!(names(&hits), vec!["notes.txt".to_string()]);
        let line = hits[0].line.as_ref().expect("该有命中行");
        assert_eq!(line.number, 2);
        assert_eq!(line.text, "second line");
    }

    #[test]
    fn content_search_keeps_the_original_case_in_the_line() {
        let root = tree();
        let hits = found(root.path(), "content:HELLO");
        assert_eq!(names(&hits), vec!["notes.txt".to_string()]);
        let line = hits[0].line.as_ref().expect("该有命中行");
        assert_eq!(line.text, "hello world");
    }

    #[test]
    fn binary_files_are_not_searched() {
        let root = TempDir::new().expect("临时目录");
        let path = root.path().join("blob.bin");
        let mut bytes = b"hello \0 world".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 100));
        fs::write(&path, bytes).expect("写文件");
        assert!(
            found(root.path(), "content:hello").is_empty(),
            "带 NUL 的文件不是文本"
        );
    }

    #[test]
    fn known_binary_extensions_are_not_read() {
        let root = tree();
        assert!(
            found(root.path(), "content:hello")
                .iter()
                .all(|hit| hit.name == "notes.txt"),
            "png 里就算有 hello 也不该被当成文本读"
        );
    }

    #[test]
    fn the_result_limit_stops_the_walk() {
        let root = TempDir::new().expect("临时目录");
        for index in 0..20 {
            write(root.path().join(format!("file{index}.txt")), "");
        }
        let search = Search::new(
            root.path().to_path_buf(),
            Query::parse("*.txt").expect("解析"),
        )
        .with_limit(3);
        let mut hits = Vec::new();
        let stats = search.walk(&AtomicBool::new(false), &mut |hit| hits.push(hit));
        assert_eq!(hits.len(), 3);
        assert!(stats.truncated);
    }

    #[test]
    fn cancelling_mid_walk_stops_it() {
        let root = TempDir::new().expect("临时目录");
        for index in 0..20 {
            write(root.path().join(format!("file{index}.txt")), "");
        }
        let cancel = AtomicBool::new(false);
        let search = Search::new(
            root.path().to_path_buf(),
            Query::parse("*.txt").expect("解析"),
        );
        let mut hits = Vec::new();
        let stats = search.walk(&cancel, &mut |hit| {
            hits.push(hit);
            // 收到第一条就中止：后面的条目根本不该再被看到。
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(hits.len(), 1);
        assert!(stats.cancelled);
    }

    #[test]
    fn an_already_cancelled_walk_does_nothing() {
        let root = tree();
        let search = Search::new(root.path().to_path_buf(), Query::parse("*").expect("解析"));
        let mut hits = Vec::new();
        let stats = search.walk(&AtomicBool::new(true), &mut |hit| hits.push(hit));
        assert!(hits.is_empty());
        assert!(stats.cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed() {
        let root = TempDir::new().expect("临时目录");
        fs::create_dir_all(root.path().join("a")).expect("建目录");
        write(root.path().join("a/inside.txt"), "");
        std::os::unix::fs::symlink(root.path().join("a"), root.path().join("a/loop"))
            .expect("建链接");
        let hits = found(root.path(), "*");
        let loops: Vec<_> = hits.iter().filter(|hit| hit.name == "loop").collect();
        assert!(loops.is_empty(), "不该跟着链接走进去：{loops:?}");
        assert_eq!(names(&hits).len(), 2, "实际：{:?}", names(&hits));
    }

    #[test]
    fn marks_are_matched_too() {
        let root = tree();
        let mut marks = Marks::default();
        marks.set_color(&root.path().join("notes.txt"), Some("red".to_string()));
        marks.set_tags(&root.path().join("src/main.rs"), vec!["工作".to_string()]);

        let search = Search::new(
            root.path().to_path_buf(),
            Query::parse("color:red").expect("解析"),
        )
        .with_marks(marks.clone());
        let mut hits = Vec::new();
        search.walk(&AtomicBool::new(false), &mut |hit| hits.push(hit));
        assert_eq!(names(&hits), vec!["notes.txt".to_string()]);

        let search = Search::new(
            root.path().to_path_buf(),
            Query::parse("tag:工作").expect("解析"),
        )
        .with_marks(marks);
        let mut hits = Vec::new();
        search.walk(&AtomicBool::new(false), &mut |hit| hits.push(hit));
        assert_eq!(names(&hits), vec!["main.rs".to_string()]);
    }

    #[test]
    fn ext_of_handles_dotfiles_and_double_extensions() {
        assert_eq!(ext_of("main.rs"), "rs");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
        assert_eq!(ext_of(".gitignore"), "");
        assert_eq!(ext_of("README"), "");
    }

    #[test]
    fn skipped_directories_are_matched_case_insensitively() {
        assert!(skipped("node_modules"));
        assert!(skipped("Node_Modules"));
        assert!(skipped("$RECYCLE.BIN"));
        assert!(!skipped("src"));
    }
}
