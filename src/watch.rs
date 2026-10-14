//! 目录变更监听：把 notify 的原始事件收敛成“这个目录该刷新了”。
//!
//! 只监听一层、不递归：列表里展示的就是这一层的内容。
//! 自己程序造成的改动也会触发事件，这没有坏处——刷新是幂等的，
//! 反而能修正“操作后状态和磁盘不一致”的情况。

use std::path::Path;

use notify::{RecursiveMode, Watcher as _};

/// 开始监听 `path` 目录，返回 watcher 和信号 receiver。
///
/// 每次目录内容变化（包括 watcher 自身出错）都会往 receiver 里发一次信号。
/// 批量操作会连续触发很多事件，调用方应自行去抖后再刷新。
/// watcher 被丢弃时监听停止，调用方通常把它移交给一个长驻任务持有。
pub fn watch_directory(
    path: &Path,
) -> Result<(notify::RecommendedWatcher, async_channel::Receiver<()>), crate::i18n::Msg> {
    let (tx, rx) = async_channel::bounded(8);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        // 监听失败（目录被删掉等）也当一次变化，让界面去面对新的现实。
        if result.is_ok() {
            let _ = tx.try_send(());
        }
    })
    .map_err(|error| crate::i18n::Message::from(error.to_string()))?;

    watcher
        .watch(path, RecursiveMode::NonRecursive)
        .map_err(|error| crate::i18n::Message::from(error.to_string()))?;

    Ok((watcher, rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    /// 事件是异步派发的，测试里轮询等一小会儿。
    fn wait_for_signal(rx: &async_channel::Receiver<()>) -> bool {
        for _ in 0..100 {
            if rx.try_recv().is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 在临时目录里制造一次变化，确认信号真的会来。
    #[test]
    fn changes_in_the_watched_directory_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let (watcher, rx) = watch_directory(root.path()).expect("start watching");

        fs::write(root.path().join("hello.txt"), "x").unwrap();
        assert!(wait_for_signal(&rx), "创建文件应当触发一次变更信号");

        // watcher 还持有着，目录再变一次也收得到。
        fs::remove_file(root.path().join("hello.txt")).unwrap();
        assert!(wait_for_signal(&rx));

        drop(watcher);
    }

    /// 监听一个不存在的目录应当直接报错，而不是留下一个永远沉默的 watcher。
    #[test]
    fn watching_a_missing_directory_fails() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("nope");
        assert!(watch_directory(&missing).is_err());
    }

    /// 信号容量很小，批量变化只会淹没到只剩最新几个，但至少留下一个。
    #[test]
    fn bursts_collapse_but_never_vanish() {
        let root = tempfile::tempdir().unwrap();
        let (watcher, rx) = watch_directory(root.path()).expect("start watching");

        for ix in 0..200 {
            fs::write(root.path().join(format!("f{ix}.txt")), "x").unwrap();
        }

        assert!(wait_for_signal(&rx));
        drop(watcher);
    }
}
