//! WFiles × NSH 客户端的 Rust 侧。
//!
//! # 这一层是什么
//!
//! **只做桥接，不实现业务逻辑**。目录模型、排序过滤、搬运、撤销、续传、
//! 特殊文件系统对象全在 `wfiles_core`（主仓库的 `crates/wfiles-core`，
//! 与 GUI 版共用同一份代码）。这里只干三件事：
//!
//! 1. 持有 `contract::Core`（浏览状态 + 内核的唯一入口）；
//! 2. 把 `Snapshot` 转成 TSX 能吃的 JSON；
//! 3. 把字符串参数转回 core 的类型（`SortKey` / `PathBuf` / 模板 id）。
//!
//! # 为什么不在这里再写一遍 `fs::read_dir`
//!
//! 上一版 PoC 的 `Fs.list` 是自写的薄实现——排序、大小文案、隐藏项、过滤
//! 全都得重写一遍。那正是这次重构要消灭的东西：**同一件事只有一份实现**，
//! GUI、TUI、NSH 三个前端看到的目录必须一模一样。
//!
//! # 线程模型（重要）
//!
//! NSH 的 `#[module]` 方法在界面的调用路径上同步执行。读一个本地目录通常
//! 是几毫秒到几十毫秒，可以接受；**搬运/删除这类可能跑几秒到几分钟的操作
//! 不能这么做**，会把界面卡住。所以操作类方法先只做"明确报错"，
//! 后台化（`async fn` + 任务）是下一阶段——见 `docs/nsh-client.md` 的进度表。
//!
//! 0.4.0 起 prelude 在 `nsh_core::prelude`（0.2.0 是 `nsh_core::ui::prelude`，
//! 升级时唯一要改的地方）。

use nsh_core::prelude::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wfiles_core::contract;
use wfiles_core::model::SortKey;
use wfiles_core::ops::ConflictPolicy;
// 远程客户端的方法都在 trait 上（`list` / `download` / `stat`），要用就得把它带进作用域。
use wfiles_core::remote::Remote as _;

/// 全局唯一的浏览内核。
///
/// `#[module]` 的方法没有 `self` 状态，只能放静态变量。`Mutex` 是必须的：
/// NSH 不保证只在同一个线程调 module 方法（测试 runner 就是另起的）。
static CORE: Mutex<Option<contract::Core>> = Mutex::new(None);

/// 内核事件的接收端。建 `Core` 时订阅一次，每次操作后 drain 出来交给界面。
static EVENTS: Mutex<Option<async_channel::Receiver<wfiles_core::kernel::KernelEvent>>> =
    Mutex::new(None);

/// 当前这趟搬运的中止手柄。
///
/// **界面线程拿它 `abort()`，绝不碰 `CORE` 的锁**：搬运期间那把锁被工作线程
/// 占着，去拿就死锁——这也是 `Core::begin_transfer()` 存在的原因，
/// 手柄必须在开工**之前**取走。
static CONTROL: Mutex<Option<wfiles_core::ops::Control>> = Mutex::new(None);

/// 最近一次进度。`None` = 现在没有搬运在跑。
static PROGRESS: Mutex<Option<Value>> = Mutex::new(None);

/// 逐项问重名时的应答口。`None` = 这一趟不逐项问（整批一个策略）。
///
/// 跟 `CONTROL` 同一套路：搬运线程在通道另一头等着，界面线程拿这个句柄答话，
/// 两边都不碰 `CORE` 的锁。**必须在搬运开始之前取走**，理由也一样。
static ANSWERS: Mutex<Option<wfiles_core::kernel::Answers>> = Mutex::new(None);

/// 当下有没有一个问题在等人回答（`ConflictHit` 攒进来，`conflict_state` 取走）。
///
/// 攒这层跟进度、搜索同理：轮询有间隔，直接让界面去读事件流的话，
/// 两轮之间到的那个问题就永远没人答，搬运一直停着。
static ASKING: Mutex<Option<Value>> = Mutex::new(None);

/// 「重名一项一项问」开关。默认**关**：批量搬一百份时逐项问一百次并不友好，
/// 而且这是本客户端唯一一个"操作会停下来等人"的模式，不该是默认行为。
static ASK_CONFLICTS: Mutex<bool> = Mutex::new(false);

/// 还没上屏的内核消息（`drain_events` 攒进来，`render` 取走）。
///
/// 为什么要中间这层：`progress()` 和 `render()` 都会 drain 同一条 channel，
/// 谁先跑谁把 `Status` 事件读走。攒在这里，等于把"读到消息"和"用掉消息"
/// 拆开，进度轮询就不会把状态栏的话吃掉。
static PENDING: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// 当前这趟搜索攒下来的命中与收尾统计。
///
/// 搜索的结果是**流式**来的（`SearchBatch` 一批批到），界面又只能轮询，
/// 所以桥接层得替它攒着：跟进度那套一样，`None` = 还没有搜过。
static SEARCH: Mutex<Option<SearchRun>> = Mutex::new(None);

/// 当前这条远程连接（SFTP）。
///
/// 与 `CORE` 分开：远程列表**不是**本地目录模型，走的是 `remote::Remote`
/// 那套（路径是相对连接根的 `/`-分隔字符串，不是 `PathBuf`）。混进内核会让
/// 本地那套"进目录/撤销/监听"全部得为远程分叉一次。
static REMOTE: Mutex<Option<Arc<dyn wfiles_core::remote::Remote>>> = Mutex::new(None);

/// 一趟搜索的账。命中先攒成 JSON，界面拿去直接画。
#[derive(Default)]
struct SearchRun {
    hits: Vec<Value>,
    /// `SearchDone` 到了没有。**没到之前别把"零命中"说成"搜完了"**。
    done: bool,
    visited: usize,
    matched: usize,
    truncated: bool,
    cancelled: bool,
}

#[native_module("wfiles/native")]
fn native(registry: &mut Registry) {
    registry.module::<Format>().module::<Wfiles>();
}

/// 展示层格式化：直接转发 `wfiles_core::format`。
///
/// 保留这个 module 是为了让界面能自己格式化数字（比如进度条上的字节数），
/// 不用为每一处都从 Rust 取一段文本。
#[module]
pub struct Format;

#[module]
impl Format {
    /// 字节数 → 「1.0 KB」。与 GUI 版同一个函数，不会出现两套格式。
    pub fn file_size(bytes: f64) -> String {
        wfiles_core::format::file_size(bytes as u64)
    }

    /// 速度 → 「2.3 MB/s」。
    pub fn speed(bytes_per_second: f64) -> String {
        wfiles_core::format::speed(bytes_per_second as u64)
    }
}

/// 文件管理器本体（桥）。
#[module]
pub struct Wfiles;

#[module]
impl Wfiles {
    // ---- 浏览 ----

    /// 打开一个目录，返回快照 JSON。第一次调用会建内核，之后就是"进目录"。
    pub fn open(path: String) -> String {
        let target = PathBuf::from(&path);
        with_core_mut(|core| {
            // 首次：建到目标目录。之后：走 Enter（会重读盘）。
            let event = core.apply(contract::Command::Enter(target));
            render(core, event)
        })
    }

    /// 重读当前目录。
    pub fn refresh() -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::Refresh))
    }

    /// 进目录。
    pub fn enter(path: String) -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::Enter(PathBuf::from(&path))))
    }

    /// 回上一级。
    pub fn up() -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::Up))
    }

    /// 过滤词（`query::Query` 的语法：通配、大小、时间都能写）。
    pub fn set_query(query: String) -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::SetQuery(query)))
    }

    /// 显示 / 不显示以 `.` 开头的项。
    pub fn set_hidden(show: bool) -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::SetHidden(show)))
    }

    /// 点某一列排序。点同一列翻转方向——规则在 `Sort::toggled` 里，别在这里重写。
    pub fn set_sort(key: String) -> String {
        let key = parse_sort_key(&key);
        with_core_mut(|core| apply_and_render(core, contract::Command::SetSort(key)))
    }

    /// 重名了怎么办：`keep_both` / `replace` / `skip`。
    ///
    /// 策略的权威值在核心里（跟链接策略同一套路），这里只是把选择发下去；
    /// 搬运时内核按它走。**问用户的方式归界面**：NSH 是一个循环切换的按钮，
    /// GUI 是冲突时弹一次窗。
    pub fn set_conflict_policy(policy: String) -> String {
        let policy = parse_conflict_policy(&policy);
        with_core_mut(|core| apply_and_render(core, contract::Command::SetConflictPolicy(policy)))
    }

    /// 重名要不要**一项一项问**（而不是整批一个策略）。
    ///
    /// 打开之后，下一趟搬运每碰到一项重名就停下来等人回答：`conflict_state()`
    /// 会给出那一个路径，界面对 `answer_conflict()` 说话。关着就是老行为。
    ///
    /// 这是本客户端唯一一个"操作会停下等人"的模式，所以默认关、并且开关状态
    /// 跟着快照一起回给界面——按钮文案不许是界面自己另存的一份。
    pub fn set_ask_conflicts(on: bool) -> String {
        *ASK_CONFLICTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = on;
        with_core_mut(|core| {
            let event = core.snapshot();
            render(core, event)
        })
    }

    /// 回答刚问到的那一项重名：`keep_both` / `replace` / `skip`。
    ///
    /// 没有问题时调也安全（幂等）：界面可能在两次轮询之间已经点过了。
    pub fn answer_conflict(policy: String) {
        let policy = parse_conflict_policy(&policy);
        // 先取走问题再答：界面靠这个信号决定要不要画那一行，
        // 答完还挂着会让它以为没答、反复重画。
        let asked = ASKING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if asked.is_none() {
            return;
        }
        let guard = ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(answers) = guard.as_ref() {
            answers.answer(policy);
        }
    }

    /// 有没有问题在等人回答，JSON：`null` 或者 `{ path, name, parent }`。
    ///
    /// 界面每 100 毫秒问一次（跟进度同一趟轮询）。
    pub fn conflict_state() -> String {
        drain_events();
        let guard = ASKING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(value) => value.to_string(),
            None => "null".to_string(),
        }
    }

    /// 看一眼这个文件会预览成什么样，JSON。
    ///
    /// **只读查询，不是命令**：不改当前目录、不进撤销栈、不推事件。所以它直接
    /// 读盘返回，没有"操作落地"那套。跟 GUI 走的是同一个 `preview::load`，
    /// 语言名与十六进制的排法都归 core，界面不维护第二份后缀表。
    ///
    /// 图片 / PDF / 音视频只回 `{"kind":"opaque","label":"图像"}`：真解码在
    /// 界面层（缩略图），而且**判类型不 open 文件**——云占位会因此触发下载。
    pub fn preview(path: String) -> String {
        use wfiles_core::contract::Preview;

        let target = PathBuf::from(&path);
        match wfiles_core::preview::load(&target) {
            Preview::Directory => json!({ "kind": "directory" }).to_string(),
            Preview::Opaque(kind) => json!({
                "kind": "opaque",
                "label": kind.label(),
                // 机器可读的类型（`image` / `pdf` / …）：界面靠它决定要不要真去
                // 解码画缩略图。**不许拿 `label` 比中文**——文案会改会本地化。
                "media": kind.id(),
            })
            .to_string(),
            Preview::Text(text) => json!({
                "kind": "text",
                "text": text.text,
                "truncated": text.truncated,
                "lines": text.lines,
                "language": text.language,
            })
            .to_string(),
            Preview::Hex(hex) => json!({
                "kind": "hex",
                // 排版是 core 的纯函数（偏移、分栏、不可打印字节），别在这里重写。
                "rows": hex.rows.iter().map(|row| row.format()).collect::<Vec<_>>(),
                "total_bytes": hex.total_bytes,
                "total_text": wfiles_core::format::file_size(hex.total_bytes),
            })
            .to_string(),
            Preview::Unavailable(reason) => json!({
                "kind": "unavailable",
                "reason": reason,
            })
            .to_string(),
        }
    }

    // ---- 两边内容差异（只读查询）----

    /// 比两个文件的内容，JSON。
    ///
    /// **只读查询**（跟 [`Wfiles::preview`] 同类）：不碰当前目录、不进撤销栈、
    /// 不推事件。文本按行比、二进制按字节比，**用哪种由 core 判**（看开头有没有
    /// NUL），界面不许按后缀猜；结论句用 `TextStats::summary()` /
    /// `HexStats::summary()`，界面只画。
    ///
    /// `limit` 是**界面**要的：一次比 8 MB 文本可能几万行，全塞进 JSON 会把
    /// 界面卡住。超出就只回前 `limit` 行，并用 `clipped` 说清楚"还有没画完的"。
    pub fn diff(left: String, right: String, char_level: bool, limit: u32) -> String {
        use wfiles_core::diff;

        let left_path = PathBuf::from(&left);
        let right_path = PathBuf::from(&right);
        let left_loaded = match diff::load(&left_path) {
            Ok(loaded) => loaded,
            Err(msg) => return json!({ "ok": false, "error": msg.to_string() }).to_string(),
        };
        let right_loaded = match diff::load(&right_path) {
            Ok(loaded) => loaded,
            Err(msg) => return json!({ "ok": false, "error": msg.to_string() }).to_string(),
        };
        let truncated = left_loaded.truncated || right_loaded.truncated;
        let limit = limit as usize;

        match (left_loaded.text.as_deref(), right_loaded.text.as_deref()) {
            (Some(left_text), Some(right_text)) => {
                let (rows, stats) = diff::diff_text(left_text, right_text, char_level);
                let clipped = rows.len() > limit;
                let rows: Vec<Value> = rows
                    .iter()
                    .take(limit)
                    .map(|row| {
                        json!({
                            // 标记就是 core 的 `Kind::label()`（" " / "改" / "左" / "右"），
                            // 界面照抄，别自己再发明一套符号。
                            "mark": row.kind.label(),
                            "left_line": row.left_line,
                            "right_line": row.right_line,
                            "left": row.left,
                            "right": row.right,
                        })
                    })
                    .collect();
                json!({
                    "ok": true,
                    "mode": "text",
                    "truncated": truncated,
                    "clipped": clipped,
                    "summary": stats.summary(),
                    "rows": rows,
                })
                .to_string()
            }
            _ => {
                let (rows, stats) = diff::diff_hex(&left_loaded.bytes, &right_loaded.bytes);
                let clipped = rows.len() > limit;
                let rows: Vec<Value> = rows
                    .iter()
                    .take(limit)
                    .map(|row| {
                        json!({
                            "offset": row.offset,
                            // 每 16 字节的排法与 ASCII 那一栏都归 core，界面不重排。
                            "left": diff::hex_line(&row.left),
                            "right": diff::hex_line(&row.right),
                            "ascii_left": diff::ascii_line(&row.left),
                            "ascii_right": diff::ascii_line(&row.right),
                        })
                    })
                    .collect();
                json!({
                    "ok": true,
                    "mode": "hex",
                    "truncated": truncated,
                    "clipped": clipped,
                    "summary": stats.summary(),
                    "rows": rows,
                })
                .to_string()
            }
        }
    }

    /// 比两个目录，JSON。只报差异（一样的那部分只计数），结论句由 core 出。
    ///
    /// **只读查询**：扫两边、不搬任何东西。真要同步是另一回事——那要走 `ops`，
    /// 有进度、有冲突策略、可撤销。这里只回答"差在哪"。
    ///
    /// `limit` 由界面给：目录大起来差异上千条，全塞进 JSON 会把界面卡住。
    pub fn compare(left: String, right: String, limit: u32) -> String {
        use wfiles_core::compare;

        let mut options = compare::Options::new(PathBuf::from(&left), PathBuf::from(&right));
        options.limit = limit as usize;
        // 一个本地扫描没有"中途叫停"的需求：界面这次查询是同步的，
        // 给个全新的手柄只为满足签名（它一直是"运行中"）。
        let control = wfiles_core::ops::Control::new();
        let mut rows: Vec<Value> = Vec::new();
        let stats = compare::compare(&options, &control, &mut |batch| {
            for entry in batch {
                rows.push(json!({
                    // 标记是 core 的 `Status::label()`（"不同" / "只有左" / "只有右"）。
                    "text": format!(
                        "{}  {}",
                        entry.status.label(),
                        entry.path.to_string_lossy()
                    ),
                }));
            }
        });
        json!({
            "ok": true,
            "mode": "dir",
            "summary": stats.summary(),
            "truncated": stats.truncated,
            "rows": rows,
        })
        .to_string()
    }

    // ---- 本机挂着的卷（只读查询）----
    /// 本机现在挂着哪些卷、各剩多少，JSON 数组。
    ///
    /// **只读查询**，跟 [`Wfiles::preview`] 同类：不是命令，不改当前目录、
    /// 不进撤销栈、不推事件。三平台扫挂载点、去重、算容量全在
    /// `wfiles_core::volumes`，这里只把结构翻成 JSON。
    ///
    /// 刻意**不经过 `CORE` 的锁**：`contract::Core::volumes()` 本身就是转发
    /// `volumes::detected()`，而搬运期间那把锁被工作线程独占——界面不该为了
    /// 一次只读查询卡到搬完。跟 `preview` 直接走 core 的公开模块是同一个取舍。
    ///
    /// `capacity_text` 由 core 的 `Capacity::describe()` 拼（`Display` 恒给中文），
    /// 界面只负责画：不许拿 `total` / `free` 自己再拼一遍。探不到就是 `null`，
    /// 界面据此**整行不画**，别把"不知道"写成"0 可用"。
    pub fn volumes() -> String {
        let volumes = wfiles_core::volumes::detected();
        let list: Vec<Value> = volumes
            .iter()
            .map(|volume| {
                json!({
                    "name": volume.name,
                    "path": volume.path.to_string_lossy(),
                    "capacity_text": volume.capacity.map(|c| c.describe().to_string()),
                })
            })
            .collect();
        Value::Array(list).to_string()
    }

    // ---- 收藏（只读查询 + 一次写）----

    /// 侧栏收藏了哪些路径，JSON 数组：`[{ path, name, is_dir }]`。
    ///
    /// 数据归 **core**（`wfiles_core::favorites`），与 GUI 版读写的是同一份
    /// `settings.json`——不是 NSH 自己另存的一份。界面上"收藏空了"就真的是空了。
    ///
    /// 名字由 `file_name()` 推，不存副本：用户把文件夹改名，收藏栏跟着变。
    pub fn favorites() -> String {
        let list: Vec<Value> = wfiles_core::favorites::list()
            .iter()
            .map(|item| {
                json!({
                    "path": item.path.to_string_lossy(),
                    "name": item
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| item.path.to_string_lossy().into_owned()),
                    "is_dir": item.is_dir,
                })
            })
            .collect();
        Value::Array(list).to_string()
    }

    /// 收藏 / 取消收藏（同一个入口点两次就是取消），返回改完之后的整份列表。
    ///
    /// 这是本客户端少数几个**写**操作之一，但它写的不是用户文件，是设置：
    /// 不进撤销栈、不推 `OperationFinished`，跟 `preview` 一样直连 core。
    pub fn toggle_favorite(path: String, is_dir: bool) -> String {
        let target = PathBuf::from(&path);
        match wfiles_core::favorites::toggle(&target, is_dir) {
            Ok(items) => {
                let list: Vec<Value> = items
                    .iter()
                    .map(|item| {
                        json!({
                            "path": item.path.to_string_lossy(),
                            "name": item
                                .path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            "is_dir": item.is_dir,
                        })
                    })
                    .collect();
                json!({ "ok": true, "items": list }).to_string()
            }
            Err(msg) => json!({ "ok": false, "error": msg.to_string() }).to_string(),
        }
    }

    // ---- 远程（SFTP）----

    /// 连一个 SFTP，成功就顺手列一下根目录。JSON。
    ///
    /// **会联网**，所以可能慢；失败时把原因**原样**回给界面（core 的 `Msg` 里
    /// 已经写了主机、端口和具体错误）——"连不上"这三个字不足以让人排查。
    ///
    /// 连接本身存在 `REMOTE` 里，后续 [`Wfiles::sftp_list`] /
    /// [`Wfiles::sftp_download`] 都用它；重连会顶掉上一条。
    pub fn sftp_connect(url: String, user: String, password: String) -> String {
        use wfiles_core::remote::{Config, Kind};

        let config = Config::new(
            "nsh-sftp".to_string(),
            "SFTP".to_string(),
            Kind::Sftp,
            url,
            user,
        );
        match wfiles_core::sftp::Sftp::new(&config, &password) {
            Ok(client) => {
                // 先列根目录再存：连上了但列不出来的连接没有用，不如当场报。
                let listed = client.list("");
                let mut guard = REMOTE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *guard = Some(Arc::new(client));
                match listed {
                    Ok(entries) => json!({
                        "ok": true,
                        "entries": entries_json(&entries),
                    })
                    .to_string(),
                    Err(msg) => json!({ "ok": false, "error": msg.to_string() }).to_string(),
                }
            }
            Err(msg) => json!({ "ok": false, "error": msg.to_string() }).to_string(),
        }
    }

    /// 列远程目录（`""` 是连接根）。没连上就如实说没连上。
    pub fn sftp_list(path: String) -> String {
        let guard = REMOTE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(client) = guard.as_ref() else {
            return json!({ "ok": false, "error": "还没连上远程" }).to_string();
        };
        match client.list(&path) {
            Ok(entries) => json!({
                "ok": true,
                "entries": entries_json(&entries),
            })
            .to_string(),
            Err(msg) => json!({ "ok": false, "error": msg.to_string() }).to_string(),
        }
    }

    /// 把远程的一个文件下到本地某个路径。返回写了多少字节。
    ///
    /// 下载**不经过内存中转**：core 的 `download` 直接往 `sink` 里写，
    /// 大文件不会把进程撑爆。
    pub fn sftp_download(path: String, dest: String) -> String {
        let guard = REMOTE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(client) = guard.as_ref() else {
            return json!({ "ok": false, "error": "还没连上远程" }).to_string();
        };
        let target = PathBuf::from(&dest);
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return json!({ "ok": false, "error": error.to_string() }).to_string();
        }
        let file = match std::fs::File::create(&target) {
            Ok(file) => file,
            Err(error) => return json!({ "ok": false, "error": error.to_string() }).to_string(),
        };
        let mut sink = std::io::BufWriter::new(file);
        match client.download(&path, &mut sink) {
            Ok(bytes) => json!({ "ok": true, "bytes": bytes }).to_string(),
            Err(msg) => {
                // 写到一半失败：别把半个文件留在那儿冒充"下载完了"。
                let _ = std::fs::remove_file(&target);
                json!({ "ok": false, "error": msg.to_string() }).to_string()
            }
        }
    }

    /// 断开（也用于"换一条连接"和测试收尾）。
    pub fn sftp_disconnect() {
        let mut guard = REMOTE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = None;
    }

    // ---- 操作 ----

    /// 复制进目标目录（重名自动留双方）。完成后可撤销。
    pub fn copy(sources: Vec<String>, dest: String) -> String {
        let sources = to_paths(sources);
        with_core_mut(|core| {
            arm_transfer(core);
            apply_and_render(
                core,
                contract::Command::Copy {
                    sources,
                    dest: PathBuf::from(&dest),
                },
            )
        })
    }

    /// 移动进目标目录。完成后可撤销。
    pub fn move_to(sources: Vec<String>, dest: String) -> String {
        let sources = to_paths(sources);
        with_core_mut(|core| {
            arm_transfer(core);
            apply_and_render(
                core,
                contract::Command::Move {
                    sources,
                    dest: PathBuf::from(&dest),
                },
            )
        })
    }

    /// 删除一批路径。
    ///
    /// `permanent = false`：尽量进回收站（可撤销）；可移动介质上会自动变成彻底删除。
    /// `permanent = true`：直接彻底删除，**不进回收站、不可撤销**。
    pub fn delete(paths: Vec<String>, permanent: bool) -> String {
        let paths = to_paths(paths);
        with_core_mut(|core| apply_and_render(core, contract::Command::Delete { paths, permanent }))
    }

    /// 移进回收站（可撤销）。可移动介质上会自动变成彻底删除。
    pub fn trash(paths: Vec<String>) -> String {
        Wfiles::delete(paths, false)
    }

    /// 同一目录内改名。
    pub fn rename(from: String, to: String) -> String {
        with_core_mut(|core| {
            apply_and_render(
                core,
                contract::Command::Rename {
                    from: PathBuf::from(&from),
                    to,
                },
            )
        })
    }

    /// 新建文件夹。
    pub fn create_folder(parent: String, name: String) -> String {
        with_core_mut(|core| {
            apply_and_render(
                core,
                contract::Command::CreateFolder {
                    parent: PathBuf::from(&parent),
                    name,
                },
            )
        })
    }

    /// 按模板新建文件（`template` 是 `plain` / `markdown` / `rust` / … 的 id）。
    pub fn create_file(parent: String, name: String, template: String) -> String {
        with_core_mut(|core| {
            apply_and_render(
                core,
                contract::Command::CreateFile {
                    parent: PathBuf::from(&parent),
                    name,
                    template,
                },
            )
        })
    }

    /// 新建文件夹时预填的名字（跳过已占用的：未命名文件夹 / 未命名文件夹 2 …）。
    pub fn fresh_folder_name(parent: String) -> String {
        wfiles_core::ops::fresh_folder_path(&PathBuf::from(&parent))
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名文件夹".to_string())
    }

    /// 新建文件时预填的名字，按模板后缀来（未命名.md / 未命名 2.md …）。
    pub fn fresh_file_name(parent: String, template: String) -> String {
        let Some(template) = wfiles_core::templates::template(&template) else {
            return "未命名.txt".to_string();
        };
        wfiles_core::ops::fresh_file_path(&PathBuf::from(&parent), template)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("未命名{}", template.extension))
    }

    /// 撤销上一步。
    pub fn undo() -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::Undo))
    }

    /// 能不能撤销（按钮可用性）。
    pub fn can_undo() -> bool {
        with_core(|core| core.can_undo()).unwrap_or(false)
    }

    /// 探针：验证 `#[module]` 支不支持 `async fn`（支持的话 TS 侧拿到的是 Promise）。
    pub async fn async_probe() -> String {
        "ok".to_string()
    }

    // ---- 异步版：可能跑很久的操作 ----
    //
    // 浏览类（open / refresh / enter / setQuery …）留同步：读一个本地目录
    // 通常几毫秒，走一趟线程池反而更慢。
    //
    // 搬运 / 删除 / 新建 / 改名 / 撤销走这些：**阻塞的部分在独立线程上跑**，
    // 界面线程只等一个 Promise。

    /// 复制（异步）。语义与 [`Wfiles::copy`] 完全一样，只是不卡界面。
    pub async fn copy_async(sources: Vec<String>, dest: String) -> String {
        let sources = to_paths(sources);
        let dest = PathBuf::from(&dest);
        on_worker(move |core| {
            arm_transfer(core);
            apply_and_render(core, contract::Command::Copy { sources, dest })
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 移动（异步）。
    pub async fn move_async(sources: Vec<String>, dest: String) -> String {
        let sources = to_paths(sources);
        let dest = PathBuf::from(&dest);
        on_worker(move |core| {
            arm_transfer(core);
            apply_and_render(core, contract::Command::Move { sources, dest })
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 现在的搬运进度，JSON：`null` = 没有搬运在跑。
    ///
    /// 界面每 100 毫秒左右问一次（NSH 没有"Rust 主动推给 JS"的路子），
    /// 每次顺便把内核事件 drain 一遍——状态栏那几句话不会因为轮到进度问就被吃掉。
    pub fn progress() -> String {
        drain_events();
        let guard = PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(value) => value.to_string(),
            None => "null".to_string(),
        }
    }

    /// 叫停正在跑的搬运。**不碰 `CORE` 的锁**，所以搬运期间调也不会卡住。
    ///
    /// 背后只是一个 `AtomicU8`：搬运线程在每个数据块之间自己问一次，
    /// 停下来之后再走正常的收尾（已完成的部分照样可撤销）。
    pub fn abort() {
        let guard = CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(control) = guard.as_ref() {
            control.abort();
        }
    }

    /// 起一趟搜索：`root` 往下走，结果**立刻返回**（回执是当前清单，没变），
    /// 命中的分批走事件流。界面每 100 毫秒问一次 [`Wfiles::search_state`]。
    ///
    /// 再调一次就是换一趟：内核会把上一趟丢掉并叫停它。
    pub fn search(root: String, query: String) -> String {
        // 先把"刚开始"的状态摆上：`render` 里的 `drain_events` 才有地方攒
        // 第一批命中。顺序反了的话第一批会被丢掉（搜得快的目录第一批就在
        // apply 里到了）。
        *SEARCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SearchRun::default());
        with_core_mut(|core| {
            apply_and_render(
                core,
                contract::Command::Search {
                    root: PathBuf::from(&root),
                    query,
                },
            )
        })
    }

    /// 叫停正在跑的那趟搜索。
    pub fn cancel_search() -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::CancelSearch))
    }

    /// 现在的搜索状态，JSON：`hits` / `done` / `visited` / `matched` /
    /// `truncated` / `cancelled`。没搜过时 `done` 也是 true（没有在跑的东西）。
    pub fn search_state() -> String {
        drain_events();
        let guard = SEARCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            None => json!({
                "done": true,
                "hits": [],
                "visited": 0,
                "matched": 0,
                "truncated": false,
                "cancelled": false,
            })
            .to_string(),
            Some(run) => json!({
                "done": run.done,
                "hits": run.hits,
                "visited": run.visited,
                "matched": run.matched,
                "truncated": run.truncated,
                "cancelled": run.cancelled,
            })
            .to_string(),
        }
    }

    /// 删除（异步）。
    pub async fn delete_async(paths: Vec<String>, permanent: bool) -> String {
        let paths = to_paths(paths);
        on_worker(move |core| {
            apply_and_render(core, contract::Command::Delete { paths, permanent })
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 重命名（异步）。
    pub async fn rename_async(from: String, to: String) -> String {
        on_worker(move |core| {
            apply_and_render(
                core,
                contract::Command::Rename {
                    from: PathBuf::from(&from),
                    to,
                },
            )
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 新建文件夹（异步）。
    pub async fn create_folder_async(parent: String, name: String) -> String {
        on_worker(move |core| {
            apply_and_render(
                core,
                contract::Command::CreateFolder {
                    parent: PathBuf::from(&parent),
                    name,
                },
            )
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 按模板新建文件（异步）。
    pub async fn create_file_async(parent: String, name: String, template: String) -> String {
        on_worker(move |core| {
            apply_and_render(
                core,
                contract::Command::CreateFile {
                    parent: PathBuf::from(&parent),
                    name,
                    template,
                },
            )
        })
        .await
        .unwrap_or_else(|| worker_died())
    }

    /// 撤销（异步）。
    pub async fn undo_async() -> String {
        on_worker(|core| apply_and_render(core, contract::Command::Undo))
            .await
            .unwrap_or_else(|| worker_died())
    }

    // ---- 辅助 ----

    /// 判据专用：钉住 core 的卷列表。传 `[]` 就是「一块盘都没挂」。
    ///
    /// **覆盖的是 `volumes::detected()` 的返回值**，不是桥接层另存一份——
    /// 所以界面拿到的仍然是 core 那条路：把 `Wfiles::volumes` 里的
    /// `detected()` 摘掉，判据会当场红。
    ///
    /// 入参形如 `[{"name":"备份盘","path":"/tmp/x","capacity":{"total":N,"free":N}}]`，
    /// `capacity: null` 表示这块盘探不出容量。归 TS 的名字是 `setTestVolumes`。
    pub fn set_test_volumes(json: String) {
        let mut volumes = Vec::new();
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&json) {
            for item in items {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let path =
                    PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or_default());
                let capacity = item.get("capacity").and_then(|value| {
                    Some(wfiles_core::volumes::Capacity {
                        total: value.get("total")?.as_u64()?,
                        free: value.get("free")?.as_u64()?,
                    })
                });
                volumes.push(wfiles_core::volumes::Volume {
                    name,
                    path,
                    capacity,
                });
            }
        }
        wfiles_core::volumes::set_override(volumes);
    }

    /// 把内核整个丢掉，下一次调用会重新建一个。
    ///
    /// **给测试用**：`CORE` 是进程级静态，同一个 runner 里跑多条测试会互相
    /// 污染（隐藏项开关、过滤词、排序方向都会留下）。界面本身不该调它。
    pub fn reset() {
        let mut guard = CORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = None;
        if let Ok(mut slot) = EVENTS.lock() {
            *slot = None;
        }
        // 搜索结果与进度都是"上一趟留下的"，跟着内核一起清掉。
        if let Ok(mut slot) = SEARCH.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = PROGRESS.lock() {
            *slot = None;
        }
        // 应答口与"在等回答的那个问题"都是上一趟的；开关也一并关掉——
        // 否则这一条测试打开的逐项问会跟着污染后面所有搬运（它们没人回答）。
        if let Ok(mut slot) = ANSWERS.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = ASKING.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = ASK_CONFLICTS.lock() {
            *slot = false;
        }
        // 卷夹具也是进程级静态：不清的话下一条判据会看见上一条钉的假盘
        //（尤其"探不出容量"那种，会把别处的容量行一起顶掉）。
        wfiles_core::volumes::clear_override();

        // 收藏写的是 `settings.json`：不隔离的话判据会往用户真实的设置里塞东西。
        // 每条测试都指向同一个进程级临时目录并清掉其中的设置文件，于是都从
        // "零收藏"开始。覆盖的是**文件在哪**，读写路径本身仍是 core 那一套。
        let state = std::env::temp_dir().join(format!("wfiles-nsh-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&state);
        let _ = std::fs::remove_file(state.join("settings.json"));
        wfiles_core::favorites::set_dir_override(Some(state));
    }

    /// 启动入口：已经有内核就只刷新，还没有就开到主目录。
    ///
    /// 界面顶层用它而不是 `open(home)`——否则 headless 测试里 `open("./App.tsx")`
    /// 会把目录强行切回 home，测试就没法在自己造的临时目录里跑
    /// （那意味着测试会往用户主目录里建文件夹）。
    pub fn boot() -> String {
        with_core_mut(|core| apply_and_render(core, contract::Command::Refresh))
    }

    /// 造一个空的临时目录，返回它的路径。测试用。
    pub fn temp_dir() -> String {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("wfiles-nsh-{}-{stamp}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        path.to_string_lossy().into_owned()
    }

    /// 往夹具目录里原样写一个文件。**只给测试用**。
    ///
    /// `create_file` 走的是「新建文件」那条命令，后缀由模板定死（.txt / .md / .rs…），
    /// 造不出 `.svg` / `.png` 这种要给缩略图判据用的东西。所以留这么一个口子：
    /// 它**不进撤销栈、不推事件、不改当前目录**，写完自己 `refresh()` 才看得见。
    pub fn write_fixture(parent: String, name: String, contents: String) -> String {
        let path = PathBuf::from(&parent).join(&name);
        match std::fs::write(&path, contents) {
            Ok(()) => json!({ "ok": true, "path": path.to_string_lossy() }).to_string(),
            Err(error) => json!({ "ok": false, "error": error.to_string() }).to_string(),
        }
    }

    /// 界面启动时的落点。
    ///
    /// 默认是用户主目录；`WFILES_HOME` 指到哪儿就开到哪儿。
    /// 要这个口子是因为真跑 `nsh run --target tui` 抓屏时（`scripts/smoke-nsh-tui.sh`），
    /// 判据得看见一个**确定**的目录——否则只能拿跑测试那台机器的主目录说事，
    /// 换台机器就红。注意不能用 `HOME` 顶替：rustup / cargo 的 toolchain 就在
    /// `$HOME` 底下，一改 nsh 连 cargo 都起不来。
    pub fn home() -> String {
        if let Ok(dir) = std::env::var("WFILES_HOME")
            && !dir.is_empty()
        {
            return dir;
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .to_string_lossy()
            .into_owned()
    }

    /// 造 n 行假数据，用来量虚拟列表的帧成本（不碰磁盘）。
    ///
    /// 保留它是因为「10 万项不掉帧」是这个项目的硬指标，而真实目录
    /// 不可能随手有几万项。数据形状与真快照一致，界面代码不用分叉。
    pub fn bench_rows(n: u32) -> String {
        let rows: Vec<Value> = (0..n)
            .map(|i| {
                let bytes = (i as u64) * 1024 + 17;
                json!({
                    "name": format!("基准文件 {:05}.dat", i),
                    "path": format!("/bench/{:05}.dat", i),
                    "is_dir": false,
                    "size": bytes,
                    "size_text": wfiles_core::format::file_size(bytes),
                    "modified": "刚刚",
                })
            })
            .collect();
        json!({
            "ok": true,
            "path": "/bench",
            "rows": rows,
            "total": rows.len(),
            "shown": rows.len(),
            "show_hidden": false,
            "query": "",
            "sort": { "key": "name", "descending": false },
            "can_undo": false,
            "messages": [],
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// 桥的里侧
// ---------------------------------------------------------------------------

/// 远程目录项 → JSON。字段与本地列表保持同一套名字（`name` / `path` / `is_dir`），
/// 界面才能用同一段代码画两种列表。
fn entries_json(entries: &[wfiles_core::remote::Entry]) -> Vec<Value> {
    entries
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "path": entry.path,
                "is_dir": entry.is_dir,
                "size": entry.size,
                "size_text": wfiles_core::format::file_size(entry.size),
            })
        })
        .collect()
}

/// 拿内核（必要时建一个指向家目录的），跑一段逻辑。
fn with_core_mut<R>(f: impl FnOnce(&mut contract::Core) -> R) -> R {
    let mut guard = CORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let core = guard.get_or_insert_with(|| {
        let start = Wfiles::home();
        let mut core = contract::Core::new(PathBuf::from(start));
        // 订阅一次就够：`Kernel` 每次 `subscribe()` 都新开一条 channel，
        // 反复订阅会漏掉前面的事件。
        if let Ok(mut slot) = EVENTS.lock() {
            *slot = Some(core.subscribe());
        }
        core
    });
    f(core)
}

/// 只读地拿内核。还没 open 过时返回 `None`。
fn with_core<R>(f: impl FnOnce(&contract::Core) -> R) -> Option<R> {
    let guard = CORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().map(f)
}

/// 开一趟搬运之前：换一个干净的中止手柄，顺手清掉上一趟的进度。
///
/// 必须在 `apply` **之前**调 —— 搬运期间 `Core` 被这一线程独占，
/// 那时候再去 `begin_transfer()` 就是自己等自己。所以调用点都紧跟在工作
/// 线程的开头，一步之隔。
fn arm_transfer(core: &mut contract::Core) {
    let control = core.begin_transfer();
    *CONTROL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control);
    *PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    // 逐项问重名的口子：开关开着才开。**也在 apply 之前**——搬运期间
    // 说这话就晚了（内核正被这一线程独占）。
    let ask = *ASK_CONFLICTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *ANSWERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = ask.then(|| core.begin_conflicts());
    *ASKING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// 在一条**独立线程**上跑一段需要内核的逻辑，等它跑完。
///
/// 为什么不用 `tokio::spawn_blocking`：这个构建把 core 的 tokio 关掉了
/// （`default-features = false`，理由见 Cargo.toml 的 sftp feature），
/// NSH 的运行时也不保证有 blocking 池。`std::thread` 最稳：一次操作一条线程，
/// 跑完就退。`async-channel` 负责把结果送回界面线程。
///
/// 注意锁的粒度：`CORE` 的锁在 `work` 里拿、在 `work` 里放，
/// `await` 的时候**不持有**，不会把界面线程卡在 `.await` 上。
async fn on_worker<R, F>(work: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce(&mut contract::Core) -> R + Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = with_core_mut(work);
        // 无视发送失败：界面已经不等了（例如窗口关了）。
        let _ = tx.send_blocking(result);
    });
    rx.recv().await.ok()
}

fn worker_died() -> String {
    json!({ "ok": false, "error": "后台线程没回话", "messages": [] }).to_string()
}

fn to_paths(items: Vec<String>) -> Vec<PathBuf> {
    items.into_iter().map(PathBuf::from).collect()
}

fn parse_sort_key(key: &str) -> SortKey {
    match key {
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        "kind" => SortKey::Kind,
        // 认不出来按名字排，别报错——手改的配置/拼错的列名不该让界面白掉。
        _ => SortKey::Name,
    }
}

fn sort_key_id(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Kind => "kind",
    }
}

/// 重名策略的字符串形式（界面用它当按钮文案与状态）。
fn conflict_policy_id(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::KeepBoth => "keep_both",
        ConflictPolicy::Replace => "replace",
        ConflictPolicy::Skip => "skip",
    }
}

fn parse_conflict_policy(policy: &str) -> ConflictPolicy {
    match policy {
        "replace" => ConflictPolicy::Replace,
        "skip" => ConflictPolicy::Skip,
        // 认不出来留在「保留两者」：这是唯一不会弄丢用户数据的一档，
        // 拼错一个词不该让粘贴变成覆盖。
        _ => ConflictPolicy::KeepBoth,
    }
}

/// 跑一条命令并渲染结果。
///
/// 单独提出来是因为 `render(core, core.apply(..))` 编译不过：
/// 两个参数同时在借 `core`（一个 `&mut`、一个 `&`）。
fn apply_and_render(core: &mut contract::Core, command: contract::Command) -> String {
    let event = core.apply(command);
    render(core, event)
}

/// 一次命令之后交给界面的东西：新快照 + 内核抛出的状态消息。
fn render(core: &contract::Core, event: contract::Event) -> String {
    drain_events();
    let messages = take_pending();
    match event {
        contract::Event::Listing(snapshot) => snapshot_json(core, &snapshot, messages),
        contract::Event::Failure { message } => json!({
            "ok": false,
            "error": message,
            "messages": messages,
        })
        .to_string(),
    }
}

fn snapshot_json(
    core: &contract::Core,
    snapshot: &contract::Snapshot,
    messages: Vec<Value>,
) -> String {
    let rows: Vec<Value> = snapshot
        .rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "path": row.path.to_string_lossy(),
                "is_dir": row.is_dir,
                "size": row.size,
                "size_text": row.size_text,
                "modified": row.modified_text,
            })
        })
        .collect();
    json!({
        "ok": true,
        "path": snapshot.path.to_string_lossy(),
        "rows": rows,
        "total": snapshot.total_count,
        "shown": snapshot.shown_count,
        "show_hidden": snapshot.show_hidden,
        "query": snapshot.query,
        "sort": {
            "key": sort_key_id(snapshot.sort.key),
            "descending": snapshot.sort.descending,
        },
        "can_undo": core.can_undo(),
        // 「下一步撤销什么」也随快照一起给：只回答"能不能撤"的话，
        // 撤销按钮就只能写「撤销」，用户闭眼按 Cmd+Z。
        //
        // 走**快照**而不是内核事件流里的 `UndoChanged.label`：事件要靠
        // `drain_events` 攒、靠轮询取，两轮之间到的那条会丢；而这里跟
        // `can_undo` 一样是当场查 core，权威且没有时序。文案由
        // `HistoryEntry::describe()` 出（`Display` 恒给中文）；要换语言的
        // 前端拿 `Msg` 自己 `say_in(locale)`。
        "undo_label": core.undo_label().map(|msg| msg.to_string()),
        // 策略的权威值在核心里：界面拿它画按钮，不自己存一份。
        "conflict_policy": conflict_policy_id(core.conflict_policy()),
        // 逐项问的开关不在核心里（内核那边它是"这一趟开没开口子"的一次性动作），
        // 所以由桥接层报给界面——但同样**只有一个来源**，界面不另存。
        "ask_conflicts": *ASK_CONFLICTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        "messages": messages,
    })
    .to_string()
}

/// 取走积压的内核事件：状态栏文字、撤销状态攒进 `PENDING`，进度更新 `PROGRESS`。
///
/// `ListingChanged` / `OperationFinished` 里除了"搬运结束"以外的东西都丢掉：
/// 这一版每次命令都返回完整快照，界面不需要增量通知。
fn drain_events() {
    use wfiles_core::kernel::KernelEvent;

    let mut batch = Vec::new();
    // 外层 `None` = 这轮没提进度；`Some(None)` = 搬运结束了。
    let mut progress: Option<Option<Value>> = None;
    {
        let guard = EVENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(receiver) = guard.as_ref() else {
            return;
        };
        while let Ok(event) = receiver.try_recv() {
            match event {
                KernelEvent::Status { message, is_error } => {
                    batch.push(json!({ "text": message, "error": is_error }));
                }
                KernelEvent::UndoChanged { available, label } => {
                    batch.push(json!({ "undo": available, "undo_label": label }));
                }
                KernelEvent::Progress {
                    label,
                    done,
                    total,
                    bytes_done,
                    bytes_total,
                } => {
                    progress = Some(Some(json!({
                        "label": label,
                        "done": done,
                        "total": total,
                        "bytes_done": bytes_done,
                        "bytes_total": bytes_total,
                        // 字节数记不出来时（没权限）界面只能按条目数显示。
                        "bytes_text": wfiles_core::format::file_size(bytes_done),
                        "bytes_total_text": wfiles_core::format::file_size(bytes_total),
                    })));
                }
                KernelEvent::OperationFinished { .. } => progress = Some(None),
                // 搬运停在这一项上等人回答。界面靠 `conflict_state()` 看到它。
                KernelEvent::ConflictHit { path } => {
                    *ASKING
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(json!({
                        "path": path.to_string_lossy(),
                        "name": path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        "parent": path
                            .parent()
                            .unwrap_or(std::path::Path::new(""))
                            .to_string_lossy(),
                    }));
                }
                KernelEvent::SearchBatch { hits } => {
                    if let Some(run) = SEARCH
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_mut()
                    {
                        run.hits.extend(hits.into_iter().map(|hit| {
                            json!({
                                "name": hit.name,
                                "path": hit.path.to_string_lossy(),
                                "parent": hit
                                    .path
                                    .parent()
                                    .unwrap_or(std::path::Path::new(""))
                                    .to_string_lossy(),
                                "is_dir": hit.is_dir,
                                "size_text": wfiles_core::format::file_size(hit.size),
                                "line": hit.line.map(|(number, text)| json!({
                                    "number": number,
                                    "text": text,
                                })),
                            })
                        }));
                    }
                }
                KernelEvent::SearchDone {
                    visited,
                    matched,
                    truncated,
                    cancelled,
                } => {
                    if let Some(run) = SEARCH
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_mut()
                    {
                        run.done = true;
                        run.visited = visited;
                        run.matched = matched;
                        run.truncated = truncated;
                        run.cancelled = cancelled;
                    }
                }
                _ => {}
            }
        }
    }
    if !batch.is_empty() {
        PENDING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(batch);
    }
    if let Some(value) = progress {
        *PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
    }
}

fn take_pending() -> Vec<Value> {
    std::mem::take(
        &mut *PENDING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}
