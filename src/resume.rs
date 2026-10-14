//! 断点续传：传了一半停下来，下一次从半路接着来。
//!
//! 思路是把"传到哪儿了"当成一趟搬运的**状态**存下来，而不是靠协议自己记：
//! 四种远程协议里只有两种有现成的续传指令，而本地拷贝连指令都没有——
//! 可它才是最需要续传的那一个（往移动硬盘上拷 40 GB，线被碰了一下）。
//!
//! 一条记录 = 一个 (源, 目标) 对。源的大小和修改时间对得上才认这本账：
//! 对不上说明源被换过了，从头传才安全。
//!
//! **半截文件不落在目标里**：数据先写进缓存目录底下的暂存文件，
//! 传成了才改名/拷贝到目标。所以用户的目标目录里要么没有这个文件，
//! 要么就是一个完整的——从来不会出现一个看着像传好了的半截货。
//!
//! 暂存是**可再生**的（源还在就能重新生成），所以放在平台惯例的缓存目录里，
//! 并且有总量预算：超了就丢最久没动的（见 [`Journal::sweep`]）。
//! 一个会无声无息写满磁盘的功能，不如没有。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(not(test))]
use crate::session;

/// 暂存数据的总预算：超过就丢最久没动的。
///
/// 为什么必须有上限：中止一个 40 GB 的拷贝会留下 40 GB 暂存，
/// 用户中止十趟就是 400 GB——没有上限的功能等于一个会写满磁盘的功能。
/// 8 GB 够接上绝大多数"传了一半"的大文件，又不至于把系统盘吃干。
const MAX_STAGING: u64 = 8 * 1024 * 1024 * 1024;

/// 正在写入中的暂存：清扫不能碰它们。
///
/// 为什么需要这么个表：暂存文件是**先建后写、账本写完才补**的，
/// 这中间有一段"文件在、账本不在"的空窗。另一个线程在这段空窗里
/// 扫过去，会把这个文件当成没人要的垃圾删掉——半截数据没了，
/// 账本却还记着"传到哪儿了"，下一次就接了个空。
/// 并行跑测试时稳定复现，真机上并发搬运时同样中招。
static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::OnceLock::new();

fn in_flight() -> &'static std::sync::Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// 这一对的暂存正在被写：拿着它，清扫就不会来动这块数据。
///
/// 做成守卫而不是"记一下、用完再划掉"是为了不漏：搬运的路上满是
/// 提前 return（出错、中止、`?`），手动配对迟早漏一次，而漏一次的
/// 后果是整块半截数据被当成垃圾清掉。
pub struct Hold {
    key: String,
}

impl Drop for Hold {
    fn drop(&mut self) {
        // 锁被人 `panic` 弄脏了也照样放行：这份表只是清扫的提示，
        // 为它再 panic 一次没有意义。
        if let Ok(mut set) = in_flight().lock() {
            set.remove(&self.key);
        }
    }
}

/// 一条续传记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// 源：本地路径，或者 `dav://<连接 id>/<子路径>`。原样存，只为看得懂。
    pub source: String,
    /// 源有多大（字节）。
    pub size: u64,
    /// 源的修改时间（Unix 纳秒）；取不到就没有。
    ///
    /// 本地源精确到纳秒；远程源受协议限制（HTTP `Last-Modified` 只到秒），
    /// 那一边就只能到秒——两者不会互相比较，一本账的源只能是本地或远程之一。
    pub modified: Option<u64>,
    /// 已经传过去的字节数——下一个起点。
    pub done: u64,
}

impl Record {
    /// 这本账还作不作数：源的大小和修改时间都得对得上，
    /// 而且不能已经传完了（传完了还接着传是白费）。
    pub fn fresh(&self, size: u64, modified: Option<u64>) -> bool {
        self.size == size && self.modified == modified && self.done < size
    }
}

/// 一个目录底下的一摞续传记录。
///
/// 建出来很便宜（就存一个路径），所以每一趟搬运现建一个就行，
/// 不用顺着调用链一路传进去。
pub struct Journal {
    root: PathBuf,
}

impl Journal {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 占住这一对的暂存；返回的守卫 drop 时自动放开。
    pub fn hold(&self, key: &str) -> Hold {
        if let Ok(mut set) = in_flight().lock() {
            set.insert(key.to_string());
        }
        Hold {
            key: key.to_string(),
        }
    }

    /// 这一对现在是不是正被写着（清扫要避开）。
    fn held(&self, key: &str) -> bool {
        in_flight()
            .lock()
            .map(|set| set.contains(key))
            .unwrap_or(false)
    }

    /// 取出某一对的记录；读不出来就当没有。
    pub fn load(&self, key: &str) -> Option<Record> {
        let text = fs::read_to_string(self.path_of(key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 记一笔。写不进去不报错：续传是锦上添花，为它打断用户不值得。
    pub fn save(&self, key: &str, record: &Record) {
        let path = self.path_of(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let Ok(json) = serde_json::to_string(record) else {
            return;
        };
        // 先写临时文件再改名：写到一半断电，也不会留下半份读不懂的账。
        let temp = path.with_extension("json.tmp");
        if fs::write(&temp, json).is_ok() {
            fs::rename(&temp, &path).ok();
        }
    }

    /// 这一对传完了（或者源变了），账本就没用了。
    pub fn clear(&self, key: &str) {
        fs::remove_file(self.path_of(key)).ok();
        fs::remove_file(Self::staging_of(&self.root, key)).ok();
    }

    /// 清扫：把再也用不上的暂存清掉，并把总量压回预算内。
    ///
    /// 三条规矩，按这个顺序：
    /// 1. **正在写的暂存一律不碰**（见 [`Self::hold`）。这一段是
    ///    "文件已经建出来、账本还没补上"的空窗，扫过去就会把别人
    ///    正在写的半截数据当垃圾删掉。
    /// 2. **没有账本的暂存一律删**。正常流程里暂存和账本同生共死，
    ///    只剩一个说明上一回写到一半进程就没了（断电、崩溃）。
    /// 3. **源已经不在的账本连锅端**。续传的前提是源还在；
    ///    源没了还留着一大截数据，纯属占地方。
    /// 4. **超预算就丢最久没动的**。暂存是按"最近用过"排序的：
    ///    用户刚中止的那一趟排在最后，所以它永远是最后被丢的那个。
    ///
    /// 返回删掉了多少字节。
    pub fn sweep(&self) -> u64 {
        let mut freed = 0u64;
        let staging_dir = self.root.join("staging");
        let Ok(entries) = fs::read_dir(&staging_dir) else {
            return 0;
        };

        // 先把账本过一遍：源不在的，连暂存一起删。
        let mut keep: Vec<(String, u64, Option<SystemTime>)> = Vec::new();
        if let Ok(journals) = fs::read_dir(&self.root) {
            for entry in journals.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|it| it != "json") {
                    continue;
                }
                let Some(key) = path.file_stem().map(|it| it.to_string_lossy().into_owned()) else {
                    continue;
                };
                // 正在写的这一对：它自己的账本可能还没落盘，别按"没有账本"判。
                if self.held(&key) {
                    continue;
                }
                let Some(record) = self.load(&key) else {
                    // 读不懂的账本：留着只会让人以为有东西可续。
                    freed += self.drop_one(&key);
                    continue;
                };
                // 远程那一项没法就地判断在不在（那要问服务器，而清扫不该联网）；
                // 只认本地路径——测试和本地拷贝是暂存的主要来源。
                let source = Path::new(&record.source);
                let remote = record.source.starts_with("remote://");
                if !remote && !source.exists() {
                    freed += self.drop_one(&key);
                    continue;
                }
                let staging = Self::staging_of(&self.root, &key);
                let (size, touched) = fs::metadata(&staging)
                    .map(|it| (it.len(), it.modified().ok()))
                    .unwrap_or((0, None));
                if size == 0 {
                    // 有账本没数据（或者反悔了传完了）：账本也没意义。
                    freed += self.drop_one(&key);
                    continue;
                }
                keep.push((key, size, touched));
            }
        }

        // 再扫暂存目录：没有账本的，删。
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                // 写账本写到一半留下的临时文件，同样没人要。
                freed += fs::metadata(entry.path()).map(|it| it.len()).unwrap_or(0);
                fs::remove_file(entry.path()).ok();
                continue;
            }
            // 正在写的这一对，它特意不在 `keep` 里（账本可能还没落盘），
            // 所以这里必须单独问一句——否则会被当成没人要的垃圾删掉，
            // 而删的正是这一趟还没传完的那半截。
            if self.held(&name) {
                continue;
            }
            if !keep.iter().any(|(key, _, _)| *key == name) {
                freed += fs::metadata(entry.path()).map(|it| it.len()).unwrap_or(0);
                fs::remove_file(entry.path()).ok();
            }
        }

        // 最后压预算：最久没动的先丢。
        let mut total: u64 = keep.iter().map(|(_, size, _)| *size).sum();
        if total <= MAX_STAGING {
            return freed;
        }
        keep.sort_by_key(|(_, _, touched)| *touched);
        for (key, size, _) in keep {
            if total <= MAX_STAGING {
                break;
            }
            freed += self.drop_one(&key);
            total = total.saturating_sub(size);
        }
        freed
    }

    /// 删掉某一对的账本和暂存，返回腾出的字节数。
    fn drop_one(&self, key: &str) -> u64 {
        let staging = Self::staging_of(&self.root, key);
        let size = fs::metadata(&staging).map(|it| it.len()).unwrap_or(0);
        fs::remove_file(&staging).ok();
        fs::remove_file(self.path_of(key)).ok();
        size
    }

    fn path_of(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    /// 暂存文件：半截数据待的地方。
    pub fn staging(&self, key: &str) -> PathBuf {
        Self::staging_of(&self.root, key)
    }

    fn staging_of(root: &Path, key: &str) -> PathBuf {
        root.join("staging").join(key)
    }
}

#[cfg(test)]
static TEST_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// 续传暂存放哪儿。
///
/// 测试构建下固定在系统临时目录（见 [`test_root`]）；正常构建优先
/// `WFILES_SESSION_DIR`（便携安装用），否则按平台惯例落在**缓存**目录：
/// macOS 是 `~/Library/Caches/WFiles`，Windows 是 `%LOCALAPPDATA%\WFiles`，
/// Linux 是 `~/.cache/wfiles`。
///
/// 为什么不是状态目录：暂存是"源还在就能重新生成"的东西，
/// 属于系统和清理工具都认的缓存语义。放在状态目录里，
/// 用户和清理工具都不敢动它——那正是它能攒到 29 GB 的原因。
pub fn root() -> PathBuf {
    #[cfg(test)]
    let dir = {
        // 测试一律走系统临时目录：真机的暂存平时在 `~/Library/Caches/WFiles`，
        // 而测试里遍地是"拷一半就叫停"——不隔开的话每跑一轮 `cargo test`
        // 就往用户的缓存目录里攒一批半截文件（前面已经攒出过 28 GB）。
        // 键是按 (源, 目标) 算的，每个测试的临时目录都不一样，共用一个目录
        // 也不会互相盖。
        test_root().to_path_buf()
    };
    #[cfg(not(test))]
    let dir = machine_root();
    dir
}

/// 真机上暂存放哪儿：优先 `WFILES_SESSION_DIR`（便携安装用），
/// 否则按平台惯例落在缓存目录。
#[cfg(not(test))]
fn machine_root() -> PathBuf {
    if let Some(dir) = session::session_dir_override() {
        return dir.join("resume");
    }
    dirs::cache_dir()
        .or_else(dirs::state_dir)
        .unwrap_or_else(|| std::env::temp_dir().join("wfiles"))
        .join("WFiles")
        .join("resume")
}

/// 测试里暂存落在哪儿：系统临时目录下一个固定位置，全进程共用。
#[cfg(test)]
pub(crate) fn test_root() -> &'static Path {
    TEST_ROOT.get_or_init(|| std::env::temp_dir().join("wfiles-test-resume"))
}

/// 这一趟的账本。
pub fn journal() -> Journal {
    Journal::new(root())
}

/// 一个 (源, 目标) 对的键。
///
/// 用哈希而不是把路径原样当文件名：路径里可能有 `/`、`:`、中文，
/// 还可能长到超出文件系统限制。取 16 字节（32 个十六进制字符）——
/// 撞不撞得上不是安全问题，是"两趟不同的搬运恰好同名"的概率问题。
pub fn key(source: &str, target: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.update([0]);
    hasher.update(target.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        out.push(hex_char(byte >> 4));
        out.push(hex_char(byte & 0x0f));
    }
    out
}

fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// 把 `staging` 落成 `target`。
///
/// 先试改名：同一个卷上是一瞬间的事，而且目标要么是旧的要么是新的，
/// 不会出现半新半旧。跨卷（移动硬盘、另一块磁盘）改名不成立，
/// 那就老实拷贝一份再把暂存的删掉。
pub fn finish(staging: &Path, target: &Path) -> Result<(), crate::i18n::Msg> {
    match fs::rename(staging, target) {
        Ok(()) => Ok(()),
        Err(rename_error) => match fs::copy(staging, target) {
            Ok(_) => {
                fs::remove_file(staging).ok();
                Ok(())
            }
            Err(_) => Err(crate::i18n::Message::args(
                "落不了盘（{from} → {to}）：{rename_error}",
                &[
                    ("from", staging.display().to_string()),
                    ("to", target.display().to_string()),
                    ("rename_error", rename_error.to_string()),
                ],
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(done: u64) -> Record {
        Record {
            source: "/from/a.txt".to_string(),
            size: 100,
            modified: Some(42),
            done,
        }
    }

    /// 一条指向真实文件的账本：清扫要靠"源还在不在"判断，所以得有个真文件。
    fn record_for(source: &str, size: u64, modified: Option<u64>, done: u64) -> Record {
        Record {
            source: source.to_string(),
            size,
            modified,
            done,
        }
    }

    /// 一个文件当前的修改时间（Unix 秒）——跟账本记的同一个口径。
    fn done_of(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .ok()
            .and_then(|it| it.modified().ok())
            .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|it| it.as_secs())
    }

    #[test]
    fn a_record_is_fresh_only_while_the_source_is_unchanged() {
        let half = record(40);
        assert!(half.fresh(100, Some(42)), "大小和时间都没变，接着传");
        assert!(!half.fresh(120, Some(42)), "源变大了：这本账不能再用");
        assert!(!half.fresh(100, Some(43)), "源被改过：同上");
        assert!(!half.fresh(100, None), "取不到时间了：宁可从头传");
        assert!(!record(100).fresh(100, Some(42)), "传完了还接什么");
    }

    #[test]
    fn a_journal_roundtrips_and_clears() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_path_buf());
        let key = key("/from/a.txt", "/to/a.txt");

        assert!(journal.load(&key).is_none(), "一开始没有账");
        journal.save(&key, &record(40));
        assert_eq!(journal.load(&key).as_ref(), Some(&record(40)));

        journal.clear(&key);
        assert!(journal.load(&key).is_none(), "传完了就该把账销掉");
    }

    #[test]
    fn a_broken_record_file_reads_as_nothing() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_path_buf());
        let key = key("/from/a.txt", "/to/a.txt");
        fs::write(journal.path_of(&key), "{ 这不是 json").unwrap();
        assert!(journal.load(&key).is_none(), "读不懂的账当作没有");
    }

    #[test]
    fn clearing_also_takes_the_staged_bytes() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_path_buf());
        let key = key("/from/a.txt", "/to/a.txt");
        let staging = journal.staging(&key);
        fs::create_dir_all(staging.parent().unwrap()).unwrap();
        fs::write(&staging, b"half").unwrap();

        journal.clear(&key);

        assert!(!staging.exists(), "半截数据跟着账本一起清");
    }

    #[test]
    fn keys_are_stable_and_differ_per_pair() {
        let a = key("/from/a.txt", "/to/a.txt");
        let b = key("/from/a.txt", "/to/a.txt");
        let c = key("/from/b.txt", "/to/a.txt");
        let d = key("/from/a.txt", "/to/b.txt");
        assert_eq!(a, b);
        assert_ne!(a, c, "换源就该是另一本账");
        assert_ne!(a, d, "换目标也该是另一本账");
        assert_eq!(a.len(), 32, "16 字节的十六进制");
    }

    #[test]
    fn a_key_survives_awkward_paths() {
        // 冒号、斜杠、中文、空格：都只是字节，不该把键撑爆或者撞车。
        let key = key("dav://jg/我的 文件:v2.txt", "/卷/目标/我的 文件.txt");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn finishing_moves_the_staged_file_into_place() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging").join("k");
        fs::create_dir_all(staging.parent().unwrap()).unwrap();
        fs::write(&staging, b"whole file").unwrap();
        let target = root.path().join("out").join("a.txt");
        fs::create_dir_all(target.parent().unwrap()).unwrap();

        finish(&staging, &target).expect("该落得下来");

        assert!(!staging.exists(), "暂存的挪走了");
        assert_eq!(fs::read(&target).unwrap(), b"whole file");
    }

    #[test]
    fn sweeping_takes_the_orphans_and_the_stale() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_path_buf());
        let alive = root.path().join("still-here.bin");
        fs::write(&alive, b"x").unwrap();

        // 一个源还在的：留着。
        let keep = key(&alive.display().to_string(), "/to/a.bin");
        let staging = journal.staging(&keep);
        fs::create_dir_all(staging.parent().unwrap()).unwrap();
        fs::write(&staging, b"half").unwrap();
        journal.save(
            &keep,
            &record_for(&alive.display().to_string(), 4, done_of(&alive), 4),
        );

        // 一个源没了的：连锅端。
        let gone = root.path().join("gone.bin");
        let stale = key(&gone.display().to_string(), "/to/b.bin");
        let stale_staging = journal.staging(&stale);
        fs::write(&stale_staging, b"half of something deleted").unwrap();
        journal.save(
            &stale,
            &record_for(&gone.display().to_string(), 100, Some(1), 24),
        );

        // 一个连账本都没有的孤儿。
        let orphan = journal.staging("deadbeef");
        fs::write(&orphan, b"nobody remembers me").unwrap();

        let freed = journal.sweep();

        assert!(journal.load(&keep).is_some(), "源还在的要留着");
        assert!(staging.exists(), "它的半截数据也留着");
        assert!(journal.load(&stale).is_none(), "源没了的账本该删");
        assert!(!stale_staging.exists(), "它的半截数据也该删");
        assert!(!orphan.exists(), "没有账本的暂存一律删");
        assert!(freed > 0, "该报腾出了多少：{freed}");
    }

    #[test]
    fn sweeping_leaves_an_empty_place_alone() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_path_buf());
        assert_eq!(journal.sweep(), 0, "什么都没有不该报错");
    }
}
