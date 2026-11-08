//! 一个只依赖标准库的文本前端（TUI 的最小接入形态）。
//!
//! 它不认识 GPUI Kit，也不引入任何界面库：只 `use wfiles_core::contract::*`，
//! 发 `Command`、拿 `Event`、订阅事件流，把行打到终端。换成 ratatui、换成别的
//! GUI 框架、加一整个新的 TUI，都不需要动 `wfiles-core` 里一行代码。
//!
//! ```text
//! cargo run --example wfiles-tui -- /path/to/start
//! ./scripts/smoke-tui.sh            # 用管道喂命令跑一遍，逐项验输出
//! ```
//!
//! 这一版刻意覆盖契约的**三种形状**，第三种最容易接错：
//!
//! 1. 普通命令：回执就是新清单（`cd` / `cp` / `undo` …）。
//! 2. **只读查询**：`preview` / `volumes` / `undo_label` / `conflict_policy`
//!    直接开 `Core` 上的方法，**不是 `Command`**——它们什么都没改，
//!    走命令会进撤销栈、推 `OperationFinished`。
//! 3. **流式命令**：`search` 立刻返回当前清单，命中分批走事件流
//!    （`SearchBatch`），收尾 `SearchDone`。**收到 `SearchDone` 之前
//!    不许把"零命中"说成"搜完了"**。
//!
//! 命令：`cd <路径>` / `up` / `hidden on|off` / `query <词>` /
//! `sort name|size|modified|kind` / `cp <源文件> <目标目录>` / `undo` /
//! `link follow|preserve` / `refresh` / `preview <路径>` / `volumes` /
//! `mkdir <名字>` / `policy keep_both|replace|skip` /
//! `search <词>` / `search-stop` / `quit`

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wfiles_core::contract::{Command, Core, Event, Preview};
use wfiles_core::entry_kind::LinkPolicy;
use wfiles_core::i18n::Locale;
use wfiles_core::kernel::KernelEvent;
use wfiles_core::model::SortKey;
use wfiles_core::ops::ConflictPolicy;

fn main() {
    let start = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // 语言由前端告诉 core；core 不读系统设置，也不读任何界面全局槽位。
    let mut core = Core::new(start).with_locale(Locale::ZhHans);
    // 每个前端一条独立订阅端：事件广播，不是抢占同一条队列。
    let events = core.subscribe();
    show(core.apply(Command::Refresh));
    drain(&events, false);
    status(&core);

    println!(
        "命令: cd <路径> | up | hidden on|off | query <词> | sort name|size|modified|kind | cp <源> <目标目录> | undo | link follow|preserve | refresh | preview <路径> | volumes | mkdir <名字> | policy keep_both|replace|skip | search <词> | search-stop | quit"
    );
    let stdin = io::stdin();
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let verb = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default().trim();

        // 只读查询不走 apply：它们不改状态，不是命令。
        match verb {
            "preview" if !rest.is_empty() => {
                show_preview(core.preview(&PathBuf::from(rest)));
                continue;
            }
            "volumes" => {
                show_volumes(&core);
                continue;
            }
            _ => {}
        }

        // `streaming` 决定要不要等 `SearchDone`：搜索在后台线程上跑，
        // 只 drain 一次会漏掉后面才到的批次。
        let (command, streaming) = match verb {
            "quit" | "exit" => break,
            "cd" if !rest.is_empty() => (Command::Enter(PathBuf::from(rest)), false),
            "up" => (Command::Up, false),
            "hidden" => (Command::SetHidden(rest == "on"), false),
            "link" => (
                Command::SetLinkPolicy(match rest {
                    "preserve" => LinkPolicy::Preserve,
                    _ => LinkPolicy::Follow,
                }),
                false,
            ),
            "query" => (Command::SetQuery(rest.to_owned()), false),
            "refresh" => (Command::Refresh, false),
            "undo" => (Command::Undo, false),
            "cp" => {
                let mut pair = rest.splitn(2, ' ');
                let from = pair.next().unwrap_or_default();
                let to = pair.next().unwrap_or_default().trim();
                if from.is_empty() || to.is_empty() {
                    println!("用法: cp <源文件> <目标目录>");
                    continue;
                }
                (
                    Command::Copy {
                        sources: vec![PathBuf::from(from)],
                        dest: PathBuf::from(to),
                    },
                    false,
                )
            }
            "sort" => (
                Command::SetSort(match rest {
                    "size" => SortKey::Size,
                    "modified" => SortKey::Modified,
                    "kind" => SortKey::Kind,
                    _ => SortKey::Name,
                }),
                false,
            ),
            "mkdir" if !rest.is_empty() => (
                Command::CreateFolder {
                    parent: core.path().to_path_buf(),
                    name: rest.to_owned(),
                },
                false,
            ),
            "policy" => (
                Command::SetConflictPolicy(match rest {
                    "replace" => ConflictPolicy::Replace,
                    "skip" => ConflictPolicy::Skip,
                    _ => ConflictPolicy::KeepBoth,
                }),
                false,
            ),
            "search" if !rest.is_empty() => (
                Command::Search {
                    root: core.path().to_path_buf(),
                    query: rest.to_owned(),
                },
                true,
            ),
            "search-stop" => (Command::CancelSearch, false),
            _ => {
                println!("不认得这条命令：{verb}");
                continue;
            }
        };
        show(core.apply(command));
        drain(&events, streaming);
        status(&core);
    }
}

/// 每趟命令之后的一行状态。两个值都是**当场问 core**，界面不另存一份：
/// 撤销标签（`History::last_describe`）与重名策略的权威值都在内核里。
fn status(core: &Core) {
    let undo = core
        .undo_label()
        .map(|label| label.to_string())
        .unwrap_or_else(|| "无".to_owned());
    println!(
        "  可撤销：{undo}   重名：{}",
        conflict_id(core.conflict_policy())
    );
}

fn conflict_id(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::KeepBoth => "keep_both",
        ConflictPolicy::Replace => "replace",
        ConflictPolicy::Skip => "skip",
    }
}

/// 打印内核事件。搜索那两类**单独排版**：结果不在回执里，前端得自己攒着画。
///
/// `wait_done` 为真时一直收到 `SearchDone` 才收工（最多等三秒），
/// 否则小目录刚开搜就 drain 完，命中与收尾统计都会漏掉。
fn drain(events: &async_channel::Receiver<KernelEvent>, wait_done: bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut done = false;
    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                KernelEvent::SearchBatch { hits } => {
                    for hit in hits {
                        let line = match &hit.line {
                            Some((number, text)) => format!("  {number}: {text}"),
                            None => String::new(),
                        };
                        println!("  [命中] {}{line}", hit.name);
                    }
                }
                KernelEvent::SearchDone {
                    visited, matched, ..
                } => {
                    println!("  搜索结束：看过 {visited} 项，命中 {matched} 项");
                    done = true;
                }
                other => println!("  [事件] {other:?}"),
            }
        }
        if !wait_done || done || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 一个卷一行。容量那句由 core 的 `Capacity::describe()` 拼，
/// 探不出来就写"容量未知"——**不许编一个 0 出来**。
fn show_volumes(core: &Core) {
    let volumes = core.volumes();
    if volumes.is_empty() {
        println!("  没有探测到其它卷");
        return;
    }
    for volume in volumes {
        match volume.capacity {
            Some(capacity) => println!("  磁盘：{}  {}", volume.name, capacity.describe()),
            None => println!("  磁盘：{}  容量未知", volume.name),
        }
    }
}

/// 预览的四种形状。语言名与十六进制行的排法都是 core 的纯函数，这里不重写。
fn show_preview(preview: Preview) {
    match preview {
        Preview::Directory => println!("  预览：目录（内容看列表）"),
        Preview::Opaque(kind) => {
            println!("  预览：{}（只报类型，真解码归界面）", kind.label());
        }
        Preview::Text(text) => {
            for line in text.text.lines().take(20) {
                println!("  | {line}");
            }
            println!(
                "  预览：{} · {} 行{}",
                text.language.unwrap_or("纯文本"),
                text.lines,
                if text.truncated {
                    "（已截断）"
                } else {
                    ""
                }
            );
        }
        Preview::Hex(hex) => {
            for row in hex.rows.iter().take(8) {
                println!("  {}", row.format());
            }
            println!("  预览：共 {} 字节", hex.total_bytes);
        }
        Preview::Unavailable(reason) => println!("  预览：读不了（{reason}）"),
    }
}

fn show(event: Event) {
    match event {
        Event::Listing(snapshot) => {
            println!(
                "{}  ({} / {} 项, 隐藏项{}, 过滤词 {:?})",
                snapshot.path.display(),
                snapshot.shown_count,
                snapshot.total_count,
                if snapshot.show_hidden { "开" } else { "关" },
                snapshot.query,
            );
            for row in snapshot.rows.iter().take(200) {
                println!(
                    "  {}  {:>10}  {}  {}",
                    if row.is_dir { "目录" } else { "文件" },
                    row.size_text,
                    row.modified_text,
                    row.name,
                );
            }
            if snapshot.rows.len() > 200 {
                println!("  … 还有 {} 项没显示", snapshot.rows.len() - 200);
            }
        }
        Event::Failure { message } => println!("错误：{message}"),
    }
}
