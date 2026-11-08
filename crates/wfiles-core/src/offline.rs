//! 离线缓存：远程目录列表落盘，断网也能翻。
//!
//! 内存里的那层（界面层远程注册表的 `listed`）只活到进程退出：拔了网线、
//! 服务器关了、公司 VPN 断了，再打开就只剩一个转圈。真正需要"离线还能翻"
//! 的是**翻目录**这件事本身——看一眼昨天有什么文件、确认某个文件在不在，
//! 不需要真把内容拉下来。
//!
//! 三条设计取舍：
//!
//! - **只缓存列表，不缓存内容。** 内容有 [`crate::resume`] 的续传暂存管，
//!   两套东西各管一段，别互相掺和。
//! - **宁可给旧数据，也不给空白。** 拿不到就回缓存里那份，并把"这是旧的"
//!   一并带出去——界面上写明列于何时，用户自己判断这个列表还能不能用。
//!   静默给一份陈的列表冒充新鲜的，比报错更坏。
//! - **缓存是可再生的，坏了就当没有。** 读不出来就当没缓存，绝不因为
//!   缓存坏了而让浏览功能整个不能用。
//!
//! 放在平台惯例的缓存目录（跟 [`crate::resume`] 同一套定位规则），
//! 并且有总量预算：用户清一次系统缓存能省下的空间，不该由这个功能占着。

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::remote::Entry;

#[cfg(not(any(test, feature = "test-support")))]
use crate::session;

/// 缓存总量预算：超过就整条丢掉最久没动的那一条连接。
///
/// 按条丢而不是按字节丢，是因为每条记录本身就很小（一屏目录几百字节到
/// 几十 KB），真要按字节精细管理，得先花力气去遍历全部记录算大小——
/// 为了省几 MB 而给每次浏览加一次全盘遍历，不划算。
/// 1 GB 意味着几万屏目录，正常人翻不到这个量。
const MAX_TOTAL: u64 = 1024 * 1024 * 1024;

/// 单条记录的上限。一条目录记录超过 16 MB 说明服务器返回了离谱的东西，
/// 直接不写——别让一个畸形响应吃掉整个预算。
const MAX_RECORD: u64 = 16 * 1024 * 1024;

/// 缓存多久算"太旧了，连翻都不该翻"。
///
/// 不是说过期就删：旧列表在断网时仍然有用（用户就是想看它）。
/// 只是久了以后占着预算不划算，所以超期的那条在清扫时被优先丢。
const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);

/// 一条记录：某个远程目录上次列出来是什么样。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    /// 列于何时（Unix 秒）。界面上要写明，用户得知道这是什么时候的。
    at: u64,
    entries: Vec<Entry>,
}

/// 落盘的东西。`entries` 不排序：列出来的顺序是服务器给的，
/// 界面自己按用户的排序偏好重排，这里存原样就对了。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// 连接 id → 该连接名下各目录的记录。
    #[serde(default)]
    by_remote: HashMap<String, HashMap<String, Record>>,
}

/// 一份离线缓存。
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// 落在哪儿。测试走系统临时目录，跟真机缓存隔开。
    pub fn root() -> PathBuf {
        #[cfg(any(test, feature = "test-support"))]
        let dir = {
            // 测试一律指到临时目录：真机的缓存是用户自己的，
            // 测试里到处是"列一个目录就存一下"，不隔开会把它们攒起来。
            take_root_override().unwrap_or_else(|| test_root().join("listing"))
        };
        #[cfg(not(any(test, feature = "test-support")))]
        let dir = machine_root();
        dir
    }

    pub fn new() -> Self {
        Self { root: Self::root() }
    }

    /// 这条缓存文件在哪儿。
    fn file(&self, id: &str) -> PathBuf {
        // 按连接 id 拆文件：一个连接的目录多到撑爆单文件时，
        // 只是它自己那份读不出来，不连累别的连接。
        // 文件名用哈希：连接名是用户自己起的，可能带 `/` 和中文。
        self.root.join(format!("{}.json", short_key(id)))
    }

    fn read(&self, id: &str) -> Cache {
        let Ok(text) = fs::read_to_string(self.file(id)) else {
            // 没有、读不了、权限不对——一律当没有。缓存坏了不该让浏览不能用。
            return Cache::default();
        };
        // 反序列化失败同理：文件可能是上一版写的，也可能被手改过。
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn write(&self, id: &str, cache: &Cache) {
        if fs::create_dir_all(&self.root).is_err() {
            return;
        }
        let Ok(text) = serde_json::to_string(cache) else {
            return;
        };
        if text.len() as u64 > MAX_RECORD {
            return;
        }
        let file = self.file(id);
        // 先写临时文件再改名：中途断电的话，留下的要么是旧的完整那份，
        // 要么什么都没有，不会是半截 JSON（那会让这条连接整个读不出来）。
        let staging = file.with_extension("json.new");
        if fs::write(&staging, &text).is_ok() && fs::rename(&staging, &file).is_err() {
            let _ = fs::remove_file(&staging);
        }
    }

    /// 记下某个远程目录刚才列出来的内容。
    pub fn save(&self, id: &str, sub: &str, entries: &[Entry]) {
        let mut cache = self.read(id);
        let record = Record {
            at: now_secs(),
            entries: entries.to_vec(),
        };
        cache
            .by_remote
            .entry(id.to_string())
            .or_default()
            .insert(sub.to_string(), record);
        self.write(id, &cache);
        self.sweep();
    }

    /// 取某个远程目录上次列出来的内容，连同"那是什么时候列的"。
    ///
    /// `None` 表示从没有列过（或者缓存坏了）。
    pub fn load(&self, id: &str, sub: &str) -> Option<(Vec<Entry>, u64)> {
        let cache = self.read(id);
        let record = cache.by_remote.get(id)?.get(sub)?;
        Some((record.entries.clone(), record.at))
    }

    /// 忘掉一条连接的全部缓存（用户删了这条连接时）。
    pub fn forget(&self, id: &str) {
        let _ = fs::remove_file(self.file(id));
    }

    /// 清掉不该留的部分。两件事，各管各的：
    ///
    /// 1. **超期的丢掉**（一个月没动）。哪怕离预算还远也丢——用户真想看就
    ///    重新连一次，而一份一个月前的列表留着纯占地方。
    /// 2. **预算还是超就丢最旧的那条**，直到进预算为止。这时连刚列的
    ///    那条也可能被丢：预算优先于新鲜，用户手动清一次缓存目录就回来了。
    fn sweep(&self) {
        let files = self.cache_files();
        let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
        for (at, len, path) in &files {
            // 最旧的那条已经写在最前面了，所以这里只管"哪条该走了"。
            let stale = SystemTime::now()
                .duration_since(*at)
                .map(|age| age > STALE_AFTER)
                .unwrap_or(false);
            if !stale && total <= MAX_TOTAL {
                // 既不超期又还在预算内，后面的只会更新，不该走。
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(*len);
            }
        }
    }

    /// 缓存目录里的缓存文件，按最旧的在前面排。
    fn cache_files(&self) -> Vec<(SystemTime, u64, PathBuf)> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
            // 只看缓存文件：写到一半的 `.json.new` 不算占用，那是瞬时的。
            .filter(|entry| entry.path().extension().is_some_and(|it| it == "json"))
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                Some((
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                    entry.path(),
                ))
            })
            .collect();
        files.sort();
        files
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// 单条目录记录里最近列过的是哪一刻（Unix 秒）。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|it| it.as_secs())
        .unwrap_or(0)
}

/// "列于 3 分钟前"这样一句话，界面上给用户判断新旧用。
pub fn ago(at: u64) -> String {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(at);
    let Ok(elapsed) = SystemTime::now().duration_since(then) else {
        return "刚刚".to_string();
    };
    match elapsed.as_secs() {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", elapsed.as_secs() / 60),
        3600..=86_399 => format!("{} 小时前", elapsed.as_secs() / 3600),
        _ => format!("{} 天前", elapsed.as_secs() / 86_400),
    }
}

/// 取一个短且稳定的文件名：连接 id 是用户起的，可能带 `/`、中文、冒号。
fn short_key(id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 真机上离线缓存放哪儿：优先 `WFILES_SESSION_DIR`（便携安装用），
/// 否则按平台惯例落在缓存目录——跟续传暂存同一套规则，
/// 用户清一次系统缓存就把两者一起清掉了。
#[cfg(not(any(test, feature = "test-support")))]
fn machine_root() -> PathBuf {
    if let Some(dir) = session::session_dir_override() {
        return dir.join("listing");
    }
    dirs::cache_dir()
        .or_else(dirs::state_dir)
        .unwrap_or_else(|| std::env::temp_dir().join("wfiles"))
        .join("WFiles")
        .join("listing")
}

// ---------------------------------------------------------------------------
// 测试隔离
// ---------------------------------------------------------------------------

/// 测试把缓存根指到临时目录用。
///
/// 为什么不给 `Store` 加个 root 参数而要一个全局覆盖：加参数就得让生产代码
/// 里每个调用点都传一遍，只为测试方便不值得。这个覆盖只在测试构建里存在。
///
/// **它是进程级的**，所以碰它的测试必须串着来——拿 [`test_guard`] 那把锁。
#[cfg(any(test, feature = "test-support"))]
static ROOT_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 碰缓存根覆盖的测试要串着来。
///
/// 两个测试各指一个目录、然后同时跑的话，会互相把对方的覆盖顶掉——
/// 症状是"单跑必过、全量并行挂"，最难查的那种。
#[cfg(any(test, feature = "test-support"))]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 指到哪儿去（`None` 是恢复默认）。
#[cfg(any(test, feature = "test-support"))]
pub fn set_root_override(dir: Option<PathBuf>) {
    *ROOT_OVERRIDE.lock().unwrap_or_else(|it| it.into_inner()) = dir;
}

/// 现在的覆盖值（守卫拿去恢复用）。
#[cfg(any(test, feature = "test-support"))]
pub fn take_root_override() -> Option<PathBuf> {
    ROOT_OVERRIDE
        .lock()
        .unwrap_or_else(|it| it.into_inner())
        .clone()
}

#[cfg(any(test, feature = "test-support"))]
fn test_root() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| std::env::temp_dir().join("wfiles-test-offline"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Entry;

    fn entry(name: &str, dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            path: name.to_string(),
            is_dir: dir,
            size: 12,
            modified: Some(1_700_000_000),
        }
    }

    /// 每个测试一个独立的缓存根：它们共用一个目录会互相盖。
    fn store(tag: &str) -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            root: dir.path().join("listing"),
        };
        // tag 留着是为了让"隔离"这件事在读代码时是显式的。
        let _ = tag;
        (store, dir)
    }

    #[test]
    fn a_saved_listing_comes_back_whole() {
        let (store, _dir) = store("save");
        let listed = vec![entry("a.txt", false), entry("子目录", true)];
        store.save("t1", "资料", &listed);

        let (back, at) = store.load("t1", "资料").expect("刚存的该读得到");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "a.txt");
        assert!(back[1].is_dir);
        assert!(at > 1_600_000_000, "该记下列于何时：{at}");
    }

    #[test]
    fn an_empty_listing_is_still_a_listing() {
        let (store, _dir) = store("empty");
        store.save("t1", "空目录", &[]);
        // 空目录也得存：断网时"这个文件夹是空的"本身就是有用信息，
        // 读不出来的话界面会去一直转圈等一个永远不来的回答。
        let (back, _) = store.load("t1", "空目录").expect("空目录也该存着");
        assert!(back.is_empty());
    }

    #[test]
    fn one_connection_does_not_leak_into_another() {
        let (store, _dir) = store("isolate");
        store.save("t1", "同一个目录", &[entry("甲.txt", false)]);
        store.save("t2", "同一个目录", &[entry("乙.txt", false)]);

        assert_eq!(store.load("t1", "同一个目录").unwrap().0[0].name, "甲.txt");
        assert_eq!(store.load("t2", "同一个目录").unwrap().0[0].name, "乙.txt");
        assert!(store.load("t1", "没列过").is_none());
    }

    #[test]
    fn sub_paths_do_not_bleed_into_each_other() {
        let (store, _dir) = store("sub");
        store.save("t1", "a", &[entry("甲.txt", false)]);
        store.save("t1", "a/b", &[entry("乙.txt", false)]);

        assert_eq!(store.load("t1", "a").unwrap().0[0].name, "甲.txt");
        assert_eq!(store.load("t1", "a/b").unwrap().0[0].name, "乙.txt");
        // 前缀相同不该互相命中：键是整串比，不是路径前缀。
        assert!(store.load("t1", "a/b/c").is_none());
    }

    #[test]
    fn a_forgotten_connection_takes_its_listings_with_it() {
        let (store, _dir) = store("forget");
        store.save("t1", "x", &[entry("甲.txt", false)]);
        store.save("t2", "x", &[entry("乙.txt", false)]);

        store.forget("t1");
        assert!(store.load("t1", "x").is_none(), "删连接就该删干净");
        assert!(store.load("t2", "x").is_some(), "别的连接不能跟着遭殃");
    }

    #[test]
    fn a_corrupted_cache_reads_as_no_cache_rather_than_failing() {
        let (store, _dir) = store("corrupt");
        store.save("t1", "x", &[entry("甲.txt", false)]);
        let file = store.file("t1");
        // 截断到一半——模拟写一半断电，或者用户手改过。
        let good = fs::read_to_string(&file).unwrap();
        fs::write(&file, &good[..good.len() / 2]).unwrap();

        assert!(
            store.load("t1", "x").is_none(),
            "坏掉的缓存当没有，但不能因此让浏览不能用"
        );
        // 还能重新写回去，一趟成功的列目录就能把坏文件覆盖掉。
        store.save("t1", "x", &[entry("新.txt", false)]);
        assert_eq!(store.load("t1", "x").unwrap().0[0].name, "新.txt");
    }

    #[test]
    fn saving_twice_replaces_rather_than_appends() {
        let (store, _dir) = store("replace");
        store.save("t1", "x", &[entry("旧的.txt", false)]);
        store.save("t1", "x", &[entry("新的.txt", false)]);

        let (back, _) = store.load("t1", "x").unwrap();
        assert_eq!(back.len(), 1, "该是替换不是追加：{back:?}");
        assert_eq!(back[0].name, "新的.txt");
    }

    #[test]
    fn nothing_to_list_reads_as_absent() {
        let (store, _dir) = store("absent");
        assert!(store.load("从没连过", "x").is_none());
    }

    #[test]
    fn a_missing_cache_directory_is_not_an_error() {
        // 根目录压根不存在（第一次用，或用户清了缓存）。
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            root: dir.path().join("压根没有"),
        };
        store.save("t1", "x", &[entry("甲.txt", false)]);
        assert_eq!(store.load("t1", "x").unwrap().0[0].name, "甲.txt");
    }

    #[test]
    fn odd_connection_names_stay_inside_the_cache_directory() {
        // 连接名是用户自己起的，带斜杠和冒号是常事。
        let (store, _dir) = store("odd");
        let weird = "公司/坚果云:主账号";
        store.save(weird, "项目/A", &[entry("方案.docx", false)]);

        assert_eq!(store.load(weird, "项目/A").unwrap().0[0].name, "方案.docx");
        // 文件名必须是这个缓存目录下的一个文件，不能是它外面某个路径。
        let file = store.file(weird);
        assert!(file.starts_with(&store.root), "逃出缓存目录了：{file:?}");
        assert_eq!(file.parent().unwrap(), store.root);
    }

    #[test]
    fn the_age_reads_as_something_a_person_can_read() {
        let now = now_secs();
        assert_eq!(ago(now), "刚刚");
        assert_eq!(ago(now - 5 * 60), "5 分钟前");
        assert_eq!(ago(now - 3 * 3600), "3 小时前");
        assert_eq!(ago(now - 2 * 86_400), "2 天前");
        // 时间戳落在未来（用户改过系统时间）不该 panic，也不该说负数。
        assert!(!ago(now + 3600).contains('-'));
    }
}
