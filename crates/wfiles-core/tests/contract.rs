//! 「新增一个前端」的验收场景：完全不碰任何界面框架，只用契约浏览目录。
//!
//! 这里模拟的就是后续 TUI（或换框架后的 GUI）会做的事：构造 `Core`、
//! 发 `Command`、拿 `Event` 渲染。整个过程中 core 的代码一行都不用改。

use std::fs;

use wfiles_core::contract::{Command, Core, Event, Snapshot};
use wfiles_core::i18n::Locale;

fn listing(event: Event) -> Snapshot {
    match event {
        Event::Listing(snapshot) => snapshot,
        Event::Failure { message } => panic!("不该失败：{message}"),
    }
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("临时目录");
    fs::create_dir(dir.path().join("a_dir")).expect("建子目录");
    fs::write(dir.path().join("b.txt"), b"bb").expect("写文件");
    fs::write(dir.path().join("c.txt"), b"ccc").expect("写文件");
    fs::create_dir(dir.path().join("sub")).expect("建可返回的子目录");
    dir
}

#[test]
fn a_new_frontend_browses_through_the_contract() {
    let dir = fixture();
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);

    let snapshot = listing(core.apply(Command::Refresh));
    assert_eq!(snapshot.total_count, 4);
    assert_eq!(snapshot.shown_count, 4);
    // WFiles 的排序约定：目录优先，不是纯字母序。新前端拿到的也是同一个结果。
    assert!(snapshot.rows[0].is_dir);
    assert_eq!(snapshot.rows[0].name, "a_dir");
    // 大小文案来自 WFiles 的 format.rs，不是前端自己算的。
    assert!(snapshot.rows.iter().any(|row| row.size_text == "2 B"));
    assert!(snapshot.rows.iter().any(|row| row.size_text == "3 B"));
}

#[test]
fn query_is_a_pure_restate_and_keeps_the_disk_total() {
    let dir = fixture();
    let mut core = Core::new(dir.path());

    let before = listing(core.apply(Command::Refresh));
    let after = listing(core.apply(Command::SetQuery("zzz-no-match".to_owned())));
    assert_eq!(after.shown_count, 0, "过滤词没生效");
    assert_eq!(
        after.total_count, before.total_count,
        "total_count 是磁盘真实总数，过滤不该把它改掉"
    );

    let hidden = listing(core.apply(Command::SetHidden(true)));
    assert!(hidden.show_hidden);
    assert_eq!(hidden.total_count, before.total_count);
}

#[test]
fn navigation_and_failure_are_ordinary_events() {
    let dir = fixture();
    let sub = dir.path().join("sub");
    let mut core = Core::new(&sub).with_locale(Locale::ZhHans);

    listing(core.apply(Command::Refresh));
    let up = listing(core.apply(Command::Up));
    assert_eq!(up.path, dir.path());
    assert!(up.rows.iter().any(|row| row.name == "a_dir"));

    match core.apply(Command::Enter(dir.path().join("does-not-exist"))) {
        Event::Failure { message } => {
            assert!(!message.is_empty(), "错误要有一句能读的话");
        }
        Event::Listing(_) => panic!("不存在的目录不该列出内容"),
    }
}

/// 「新增一个 TUI」真正要复用的卖点：复制之后能撤销，而且状态变化经事件流
/// 推给订阅者。撤销栈只有一份（`Kernel` 持有），不在这个前端里重建。
#[test]
fn copy_then_undo_goes_through_the_one_kernel() {
    use wfiles_core::kernel::KernelEvent;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    let source = source_dir.join("a.txt");
    fs::write(&source, b"hello").expect("写源文件");

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    assert!(!core.can_undo(), "一开始没东西可撤");

    match core.apply(Command::Copy {
        sources: vec![source.clone()],
        dest: dest.clone(),
    }) {
        Event::Listing(snapshot) => assert_eq!(snapshot.rows.len(), 1, "副本该出现在目标目录"),
        Event::Failure { message } => panic!("复制失败：{message}"),
    }
    assert!(core.can_undo(), "复制之后该能撤销");
    assert!(dest.join("a.txt").exists(), "目标目录里该有副本");

    // 事件流里必须同时看到三类变化，且只有一个订阅者也能收到全部。
    let mut saw_operation = false;
    let mut saw_undo = false;
    let mut saw_listing = false;
    while let Ok(event) = events.try_recv() {
        match event {
            KernelEvent::OperationFinished { .. } => saw_operation = true,
            KernelEvent::UndoChanged { available, .. } => saw_undo |= available,
            KernelEvent::ListingChanged { .. } => saw_listing = true,
            KernelEvent::Status { .. } => {}
            KernelEvent::LinkPolicyChanged { .. } => {}
            // 偏好类变化另有专测（链接策略、冲突策略各一条）。
            KernelEvent::ConflictPolicyChanged { .. } => {}
            // 进度是高频事件，这条判据只管"三类状态变化有没有到"，进度另有专测。
            KernelEvent::Progress { .. } => {}
            // 搜索有自己的一套判据（见文件末尾），这条只数搬运。
            KernelEvent::SearchBatch { .. } | KernelEvent::SearchDone { .. } => {}
            // 逐项问重名另有专测（`a_new_frontend_answers_each_conflict_one_by_one`）。
            KernelEvent::ConflictHit { .. } => {}
        }
    }
    assert!(
        saw_operation && saw_undo && saw_listing,
        "事件流漏了：operation={saw_operation}, undo={saw_undo}, listing={saw_listing}"
    );

    match core.apply(Command::Undo) {
        Event::Listing(snapshot) => assert_eq!(snapshot.rows.len(), 0, "撤销后副本该消失"),
        Event::Failure { message } => panic!("撤销失败：{message}"),
    }
    assert!(!core.can_undo(), "撤销栈该空了");
    assert!(!dest.join("a.txt").exists(), "副本该被删掉");
    assert!(source.exists(), "源文件不能被撤销误删");

    // 撤销也把事件推给了订阅者（含一条 Status）。
    let mut saw_status = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, KernelEvent::Status { .. }) {
            saw_status = true;
        }
    }
    assert!(saw_status, "撤销结论该推给订阅者");
}

/// 链接策略是核心状态、不是前端各自的一份偏好：新前端只发一条命令，
/// 同一条 Copy 链路就得换行为。判据盯的是"副本到底是不是链接"——
/// 把 SetLinkPolicy 的接线摘掉、或 transfer 忘了用 options，这里都会红。
#[cfg(unix)]
#[test]
fn a_new_frontend_can_choose_how_links_are_copied() {
    use std::os::unix::fs::symlink;

    use wfiles_core::entry_kind::LinkPolicy;
    use wfiles_core::kernel::KernelEvent;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    let real = source_dir.join("real.txt");
    fs::write(&real, b"hello").expect("写目标文件");
    let link = source_dir.join("link.txt");
    symlink(&real, &link).expect("建符号链接");

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();

    // 默认跟随：副本是普通文件，内容等于链接指向的那份。
    match core.apply(Command::Copy {
        sources: vec![link.clone()],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("默认复制失败：{message}"),
    }
    let first = dest.join("link.txt");
    assert!(
        !fs::symlink_metadata(&first)
            .expect("副本在")
            .file_type()
            .is_symlink(),
        "默认跟随，副本不该还是链接"
    );
    assert_eq!(fs::read(&first).expect("读副本"), b"hello");
    while events.try_recv().is_ok() {}

    // 改成保留链接：事件流要广播这一变化。
    assert!(matches!(
        core.apply(Command::SetLinkPolicy(LinkPolicy::Preserve)),
        Event::Listing(_)
    ));
    let mut saw_policy = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, KernelEvent::LinkPolicyChanged { preserve: true }) {
            saw_policy = true;
        }
    }
    assert!(saw_policy, "策略变化该推给订阅者");

    match core.apply(Command::Copy {
        sources: vec![link],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("保留链接复制失败：{message}"),
    }
    // 重名走 KeepBoth，落点是 "link 2.txt"。
    let second = dest.join("link 2.txt");
    assert!(
        fs::symlink_metadata(&second)
            .expect("第二个副本在")
            .file_type()
            .is_symlink(),
        "选了保留链接，副本就该是链接本身"
    );
    assert_eq!(fs::read_link(&second).expect("读链接目标"), real);
}

/// 新建 / 重命名也要能从契约层走，并且**都能撤销**。
///
/// 判据盯的是磁盘：文件真的出现了、内容真的来自模板、撤销之后真的没了。
/// 只断言"回了个 Listing"是挑错层——那种判据在实现完全空转时也绿。
#[test]
fn a_new_frontend_can_create_and_rename_through_the_contract() {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let parent = dir.path().to_path_buf();

    match core.apply(Command::CreateFolder {
        parent: parent.clone(),
        name: "sub".to_owned(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("新建文件夹失败：{message}"),
    }
    assert!(parent.join("sub").is_dir(), "文件夹该真的建出来");

    match core.apply(Command::CreateFile {
        parent: parent.clone(),
        name: "readme.md".to_owned(),
        template: "markdown".to_owned(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("新建文件失败：{message}"),
    }
    assert_eq!(
        fs::read_to_string(parent.join("readme.md")).expect("读新文件"),
        "# 标题\n\n",
        "内容该来自模板，不是一个空文件"
    );

    // 故意改成**真正不同的名字**，不用纯大小写（`readme.md` → `README.md`）：
    // macOS 的 APFS 默认大小写不敏感，那种改名之后两个路径都 `exists()`，
    // 判据会变成在测文件系统而不是在测重命名。
    match core.apply(Command::Rename {
        from: parent.join("readme.md"),
        to: "notes.md".to_owned(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("重命名失败：{message}"),
    }
    assert!(parent.join("notes.md").exists());
    assert!(!parent.join("readme.md").exists(), "旧名字该没了");

    // 撤销是后进先出：先恢复原名，再删掉新建的那个文件。
    core.apply(Command::Undo);
    assert!(parent.join("readme.md").exists(), "撤销重命名该恢复原名");
    core.apply(Command::Undo);
    assert!(!parent.join("readme.md").exists(), "撤销新建该把文件删掉");
    assert!(parent.join("sub").is_dir(), "更早建的那个文件夹不该被连累");
}

#[test]
fn an_unknown_template_is_rejected_with_a_readable_message() {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);

    match core.apply(Command::CreateFile {
        parent: dir.path().to_path_buf(),
        name: "x.md".to_owned(),
        template: "no-such-template".to_owned(),
    }) {
        Event::Failure { message } => assert!(!message.is_empty(), "拒绝要给一句能读的话"),
        Event::Listing(_) => panic!("不认识的模板不该成功"),
    }
}

/// 彻底删除走契约层时：文件没了，而且**不能撤销**。
#[test]
fn a_permanent_delete_through_the_contract_leaves_nothing_to_undo() {
    let dir = tempfile::tempdir().expect("临时目录");
    let file = dir.path().join("gone.txt");
    fs::write(&file, b"x").expect("写文件");

    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    match core.apply(Command::Delete {
        paths: vec![file.clone()],
        permanent: true,
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("彻底删除失败：{message}"),
    }
    assert!(!file.exists(), "文件该真的没了");
    assert!(!core.can_undo(), "彻底删除不该留下可撤销状态");
}

/// 搬运要能报进度：新前端不靠回调，只靠事件流就能画进度条。
///
/// 判据是**事件序列本身**：`done` 只增不减、最后一个 `done == total`。
/// 只断言"收到过一条 Progress"是挑错层——实现只在开头发一条也能绿。
#[test]
fn progress_events_come_out_of_a_copy() {
    use wfiles_core::kernel::KernelEvent;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    let names = ["a.txt", "b.txt", "c.txt"];
    for name in names {
        fs::write(source_dir.join(name), vec![b'x'; 4096]).expect("写源文件");
    }
    let sources: Vec<_> = names.iter().map(|name| source_dir.join(name)).collect();

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    // 手柄得在搬运之前取走：搬运期间 `Core` 被调用线程独占。
    let _control = core.begin_transfer();
    match core.apply(Command::Copy {
        sources,
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("复制失败：{message}"),
    }

    let mut sightings = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let KernelEvent::Progress {
            done,
            total,
            bytes_done,
            bytes_total,
            ..
        } = event
        {
            sightings.push((done, total, bytes_done, bytes_total));
        }
    }
    assert!(
        sightings.len() >= names.len(),
        "每一项都该有一次进度，实际 {} 次",
        sightings.len()
    );
    let (_, total, _, bytes_total) = *sightings.last().expect("至少一条进度");
    assert_eq!(total, names.len(), "总条目数该是这次要搬的项数");
    assert_eq!(bytes_total, 3 * 4096, "总字节数该是三个文件之和");
    // `done` 单调不减，且最后一项之后是 total。
    let mut previous = 0;
    for (done, ..) in &sightings {
        assert!(*done >= previous, "done 不该倒退：{sightings:?}");
        previous = *done;
    }
    assert_eq!(previous, total, "搬完了 done 该等于 total");
    // 字节也是真的在涨（不是永远 0 的形状）。
    assert!(
        sightings.iter().any(|(.., bytes, _)| *bytes > 0),
        "bytes_done 该有实际数字"
    );
}

/// 重名场景的夹具：源和目标各有一份 `a.txt`，内容不同。
///
/// 判据要盯**内容**而不是"有没有报错"：KeepBoth 和 Replace 都算"成功"，
/// 只有读回磁盘才知道到底走了哪条路。
fn conflict_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    fs::write(source_dir.join("a.txt"), b"new").expect("写源文件");
    fs::write(dest.join("a.txt"), b"old").expect("写目标文件");
    let source = source_dir.join("a.txt");
    (dir, source, dest)
}

/// 冲突策略是核心状态，不是前端各自的一份偏好：新前端发一条命令，
/// 同一条 Copy 链路就得换行为。
///
/// 判据盯磁盘内容——只断言"回了个 Listing"的话，策略压根没被 transfer
/// 用上也是绿的（那就成了挑错层）。
#[test]
fn replace_overwrites_the_existing_file() {
    use wfiles_core::kernel::KernelEvent;
    use wfiles_core::ops::ConflictPolicy;

    let (_dir, source, dest) = conflict_fixture();
    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();

    match core.apply(Command::SetConflictPolicy(ConflictPolicy::Replace)) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("改策略不该失败：{message}"),
    }
    let mut saw_policy = false;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event,
            KernelEvent::ConflictPolicyChanged {
                policy: ConflictPolicy::Replace
            }
        ) {
            saw_policy = true;
        }
    }
    assert!(saw_policy, "策略变化该推给订阅者，否则前端的开关对不上");

    match core.apply(Command::Copy {
        sources: vec![source],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("替换式复制失败：{message}"),
    }
    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"new",
        "选了替换，目标内容该变成源的"
    );
    assert!(!dest.join("a 2.txt").exists(), "替换不该再留一份");
}

/// 选了跳过：目标原封不动，**而且状态栏要说话**。
///
/// 跳过是唯一一种"命令成功、磁盘上什么都没有"的结果。不报一句就是
/// 静默失败——用户点了确定，界面上什么都没发生。
#[test]
fn skip_leaves_the_target_untouched_and_says_so() {
    use wfiles_core::kernel::KernelEvent;
    use wfiles_core::ops::ConflictPolicy;

    let (_dir, source, dest) = conflict_fixture();
    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();

    assert!(matches!(
        core.apply(Command::SetConflictPolicy(ConflictPolicy::Skip)),
        Event::Listing(_)
    ));
    match core.apply(Command::Copy {
        sources: vec![source],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("跳过不该报成失败：{message}"),
    }

    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"old",
        "跳过就该一个字节都不动"
    );
    assert!(!dest.join("a 2.txt").exists(), "跳过不该另存一份");
    assert!(!core.can_undo(), "什么都没搬，不该留下可撤销记录");

    let mut said = String::new();
    while let Ok(event) = events.try_recv() {
        if let KernelEvent::Status { message, .. } = event
            && message.contains("跳过")
        {
            said = message;
        }
    }
    assert!(said.contains("1 项"), "该说清跳过了几项，实际：{said:?}");
}

/// 不设策略时默认「保留两者」——新前端不必先设一遍才拿到安全行为。
///
/// 这条同时是上面两条的**对照**：同一个夹具、同一条 Copy 链路，
/// 只差一条 `SetConflictPolicy`。它绿而上面红，说明红的是策略而不是夹具。
#[test]
fn the_default_policy_keeps_both_copies() {
    let (_dir, source, dest) = conflict_fixture();
    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);

    match core.apply(Command::Copy {
        sources: vec![source],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("默认复制失败：{message}"),
    }
    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读原目标"),
        b"old",
        "默认不该覆盖已有的那份"
    );
    assert_eq!(
        fs::read(dest.join("a 2.txt")).expect("读新副本"),
        b"new",
        "默认该另存一份"
    );
}

/// 逐项问冲突：两项都重名，第一项答「替换」、第二项答「跳过」，
/// 磁盘结果必须**一项一项不一样**。
///
/// 这正是"整批一个策略"做不到的事：同一批里既换了一份又留住另一份。
/// 判据盯磁盘内容——只断言"收到过 ConflictHit"是挑错层（问是问了，
/// 答案压根没被用上也是绿的）。
#[test]
fn a_new_frontend_answers_each_conflict_one_by_one() {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use wfiles_core::kernel::KernelEvent;
    use wfiles_core::ops::ConflictPolicy;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    // 两项都重名，且内容不同：只有这样才分得清"换了"还是"留住了"。
    for name in ["a.txt", "b.txt"] {
        fs::write(source_dir.join(name), b"new").expect("写源文件");
        fs::write(dest.join(name), b"old").expect("写目标文件");
    }

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    // 两个把手都得在发 Copy **之前**取走：搬运期间 `Core` 被调用线程独占。
    let answers = core.begin_conflicts();
    let _control = core.begin_transfer();

    // 搬运会阻塞在问题上等人回话，所以应答得在另一条线程上做——
    // 这正是真实前端的样子（界面线程答、搬运线程等）。
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&asked);
    let reply = answers.clone();
    let answering = std::thread::spawn(move || {
        // 有截止时间：实现坏掉时不至于把整个测试套挂死。
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                match event {
                    KernelEvent::ConflictHit { path } => {
                        let mut asked = seen.lock().expect("锁");
                        asked.push(
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        );
                        // 第一项替换、第二项跳过——顺序由搬运自己决定，
                        // 前端只按问到的先后答（一问一答，按顺序配对）。
                        let policy = if asked.len() == 1 {
                            ConflictPolicy::Replace
                        } else {
                            ConflictPolicy::Skip
                        };
                        reply.answer(policy);
                    }
                    KernelEvent::OperationFinished { .. } => return,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    match core.apply(Command::Copy {
        sources: vec![source_dir.join("a.txt"), source_dir.join("b.txt")],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("复制失败：{message}"),
    }
    answering.join().expect("应答线程");

    assert_eq!(
        asked.lock().expect("锁").as_slice(),
        ["a.txt", "b.txt"],
        "两项都该问一次，且按搬运顺序问"
    );
    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"new",
        "第一项答了替换，内容该变成源的"
    );
    assert_eq!(
        fs::read(dest.join("b.txt")).expect("读目标"),
        b"old",
        "第二项答了跳过，该一个字节都不动"
    );
    assert!(!dest.join("a 2.txt").exists(), "替换不该再留一份");
    assert!(!dest.join("b 2.txt").exists(), "跳过不该另存一份");
}

/// 不取应答口就是老行为：整批一个策略，一次都不问。
///
/// 这条是上面那条的**对照**——同一个夹具、同一条 Copy 链路，只差
/// `begin_conflicts()`。它绿而上面红，说明红的是"逐项问"而不是夹具。
#[test]
fn without_the_answer_handle_nothing_is_asked() {
    use wfiles_core::kernel::KernelEvent;
    use wfiles_core::ops::ConflictPolicy;

    let (_dir, source, dest) = conflict_fixture();
    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    assert!(matches!(
        core.apply(Command::SetConflictPolicy(ConflictPolicy::Skip)),
        Event::Listing(_)
    ));

    match core.apply(Command::Copy {
        sources: vec![source],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("复制失败：{message}"),
    }
    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"old",
        "没取应答口，就该按整批策略跳过"
    );
    let mut asked = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, KernelEvent::ConflictHit { .. }) {
            asked += 1;
        }
    }
    assert_eq!(asked, 0, "没取应答口就不该推任何 ConflictHit");
}

/// 停在一个问题上等人回答时，按中止也得停得下来。
///
/// 这条盯的是"等待"那段代码的收尾口：只在两个条目之间检查中止是不够的，
/// 用户完全可能是在看到对话框之后才决定不搬了。
///
/// 源文件**故意是空的**：每一项复制时、以及每写完一个数据块时都会上报一次进度，
/// 而进度上报本身就盯着中止位。源里只要有一个字节，那道闸就会先一步拦住，
/// 这条判据把"等待期间的中止检查"摘掉也照样绿（挑错层）。空文件没有数据块，
/// 等待期间的那次检查才是唯一防线。
#[test]
fn aborting_while_a_conflict_is_pending_stops_the_transfer() {
    use std::time::{Duration, Instant};

    use wfiles_core::kernel::KernelEvent;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    for name in ["a.txt", "b.txt"] {
        fs::write(source_dir.join(name), b"").expect("写空源文件");
        fs::write(dest.join(name), b"old").expect("写目标文件");
    }

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    let answers = core.begin_conflicts();
    let control = core.begin_transfer();

    // 看到第一个问题就按中止，一个问题都不答。
    let stopping = control.clone();
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                if matches!(event, KernelEvent::ConflictHit { .. }) {
                    stopping.abort();
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    match core.apply(Command::Copy {
        sources: vec![source_dir.join("a.txt"), source_dir.join("b.txt")],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("中止不该报成失败：{message}"),
    }
    watcher.join().expect("观察线程");
    // 一直没答，说明问过那一次之后就没再往下走。
    drop(answers);

    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"old",
        "中止了就一项都不该动"
    );
    assert_eq!(fs::read(dest.join("b.txt")).expect("读目标"), b"old");
    assert!(
        !dest.join("a 2.txt").exists(),
        "第一个问题被中止了，这一项就不该再按默认策略搬进来"
    );
    assert!(!core.can_undo(), "什么都没搬，不该留下可撤销记录");
}

/// 应答口被丢掉了（前端崩了 / 换了一趟）不能把搬运挂死：退回整批策略把活干完。
///
/// 反过来说：这条绿就证明了 `Answers` 不是"必须有人回答否则永远阻塞"。
#[test]
fn a_lost_answer_handle_falls_back_to_the_batch_policy() {
    let (_dir, source, dest) = conflict_fixture();
    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    // 开了口子又立刻丢掉——没人会回答。
    drop(core.begin_conflicts());

    match core.apply(Command::Copy {
        sources: vec![source],
        dest: dest.clone(),
    }) {
        Event::Listing(_) => {}
        Event::Failure { message } => panic!("复制失败：{message}"),
    }
    assert_eq!(
        fs::read(dest.join("a.txt")).expect("读目标"),
        b"old",
        "没人回答时该按默认策略（保留两者），不能覆盖"
    );
    assert_eq!(
        fs::read(dest.join("a 2.txt")).expect("读新副本"),
        b"new",
        "退回默认策略之后仍要把活干完"
    );
}

// ---- 预览：只读查询，不是命令 ----
//
// 预览是"看一眼"：读最多 256 KB、同步返回、不排队、不广播。
// 所以它在契约里是 `Core::preview(&path)` 而不是一条 `Command`——
// 命令会走"操作落地"那条路（进撤销栈、推 OperationFinished），
// 而看内容什么都没改。

/// 四种形状各验一次，外加"看的时候什么都没动"。
#[test]
fn a_new_frontend_previews_without_touching_anything() {
    use wfiles_core::contract::{Kind, Preview};

    let dir = tempfile::tempdir().expect("临时目录");
    let source = dir.path().join("main.rs");
    fs::write(&source, b"fn main() {\n    println!(\"hi\");\n}\n").expect("写源码");
    let binary = dir.path().join("blob.bin");
    fs::write(&binary, [0u8, 1, 2, 3, 0xff, 0xfe, b'A', b'B']).expect("写二进制");
    let image = dir.path().join("photo.png");
    fs::write(&image, b"not-really-a-png").expect("写假图片");
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).expect("建子目录");

    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    let before = listing(core.apply(Command::Refresh));

    // 源码：内容原样回来，语言来自 core 的表（界面不该自己再维护一份后缀表）。
    match core.preview(&source) {
        Preview::Text(text) => {
            assert_eq!(text.text, "fn main() {\n    println!(\"hi\");\n}");
            assert_eq!(text.language, Some("Rust"), "语言名该由 core 认出来");
            assert!(!text.truncated, "这么点内容不该标截断");
            assert_eq!(text.lines, 3);
        }
        other => panic!("源码该预览成文本：{other:?}"),
    }

    // 二进制：不是文本就转十六进制，行首是偏移（`HexRow::format` 归 core）。
    match core.preview(&binary) {
        Preview::Hex(hex) => {
            assert_eq!(hex.total_bytes, 8);
            let first = hex.rows.first().expect("该有一行").format();
            assert!(first.starts_with("00000000  "), "行首该是偏移：{first:?}");
            assert!(first.contains("00 01 02 03"), "字节该按顺序排：{first:?}");
            assert!(first.ends_with("AB"), "不可打印字节写成点：{first:?}");
        }
        other => panic!("二进制该走十六进制：{other:?}"),
    }

    // 图片：只按扩展名给占位，**绝不读内容**（那是解码器的活，也是云占位
    // 绝不能被"判类型"顺手下载的那条规矩）。
    assert_eq!(core.preview(&image), Preview::Opaque(Kind::Image));
    // 空 .png 里其实有 16 字节文本，要是读了内容就会变成 Hex/Text——上面那句就是判据。
    assert_eq!(core.preview(&sub), Preview::Directory);

    // 看了一圈，什么都没变：清单没重读、没有新事件、撤销栈仍是空的。
    let after = listing(core.apply(Command::Refresh));
    assert_eq!(after, before, "预览不该改动当前清单");
    assert!(!core.can_undo(), "预览不是操作，不该产生撤销记录");
    let mut strays = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            // 两次 Refresh 自己那几条事件是正常的。
            wfiles_core::kernel::KernelEvent::ListingChanged { .. }
            | wfiles_core::kernel::KernelEvent::UndoChanged { .. }
            | wfiles_core::kernel::KernelEvent::Status { .. } => {}
            other => strays.push(format!("{other:?}")),
        }
    }
    assert!(strays.is_empty(), "预览不该推任何事件：{strays:?}");
}

// ---- 卷：只读查询，三平台目录约定归 core ----
//
// 新前端要画"磁盘"那一栏时最不该重写的东西：macOS 挂 `/Volumes`、Windows 扫盘符、
// Linux 散在 `/media/<用户>`——那是三个平台的目录约定，不是界面知识。

/// 契约给的必须**就是** core 扫出来的那一份（不是另算一遍），
/// 而且容量的"探不出来"必须是 `None` 而不是 0。
#[test]
fn a_new_frontend_lists_volumes_through_the_contract() {
    use wfiles_core::contract::{Capacity, Volume};
    use wfiles_core::volumes;

    const GB: u64 = 1024 * 1024 * 1024;
    // 跑测试的机器上不一定插着盘，钉一个假的才判得出来。
    let _serial = volumes::test_guard();
    volumes::set_override(vec![
        Volume {
            name: "备份盘".to_owned(),
            path: "/Volumes/Backup".into(),
            capacity: Some(Capacity {
                total: 500 * GB,
                free: 120 * GB,
            }),
        },
        Volume {
            name: "光盘".to_owned(),
            path: "/Volumes/CD".into(),
            // 光盘、网络共享、空读卡器都可能报不出容量。
            capacity: None,
        },
    ]);

    let core = Core::new("/");
    let listed = core.volumes();
    let direct = volumes::detected().as_ref().clone();
    volumes::clear_override();

    assert_eq!(listed, direct, "契约该转发 core 扫出来的那一份，不自己另算");
    assert_eq!(listed.len(), 2);
    // 容量是 `Option`：界面据此决定显不显示那一行，**别把"不知道"画成"满了"**。
    assert_eq!(listed[0].capacity.map(|it| it.free), Some(120 * GB));
    assert_eq!(listed[1].capacity, None);
    // 那行字也由 core 拼（GUI 侧栏与将来的 TUI 状态栏要说同一句）。
    assert_eq!(
        listed[0]
            .capacity
            .expect("第一块盘有容量")
            .describe()
            .say_in(Locale::ZhHans),
        "可用 120.0 GB / 共 500.0 GB"
    );
}

/// 搬运前就把中止位置上：一个文件都不该被碰，而且**要说清是被叫停的**。
#[test]
fn aborting_before_the_transfer_starts_touches_nothing() {
    use wfiles_core::kernel::KernelEvent;

    let dir = tempfile::tempdir().expect("临时目录");
    let source_dir = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir(&source_dir).expect("建源目录");
    fs::create_dir(&dest).expect("建目标目录");
    fs::write(source_dir.join("a.txt"), b"hello").expect("写源文件");

    let mut core = Core::new(&dest).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    core.begin_transfer().abort();

    match core.apply(Command::Copy {
        sources: vec![source_dir.join("a.txt")],
        dest: dest.clone(),
    }) {
        // 中止不是"失败"：命令是被接受的，只是没搬东西。
        Event::Listing(snapshot) => assert_eq!(snapshot.rows.len(), 0, "目标目录该还是空的"),
        Event::Failure { message } => panic!("中止不该报成失败：{message}"),
    }
    assert!(!dest.join("a.txt").exists(), "中止了就不该落盘");
    assert!(!core.can_undo(), "什么都没做，就不该有可撤销的记录");

    let mut said = String::new();
    while let Ok(event) = events.try_recv() {
        if let KernelEvent::Status { message, .. } = event
            && message.contains("中止")
        {
            said = message;
        }
    }
    assert!(
        said.contains("0 项"),
        "该说清「中止了、完成 0 项」，实际：{said:?}"
    );
}

// ---- 搜索：结果走事件流，不走回执 ----
//
// 搜索要走整棵树、可能还要读文件内容，**不能**让 `apply` 等它（那是界面卡死）。
// 所以回执只说"收下了"，命中分批发事件流。判据盯着批次内容与收尾统计——
// 只断言"收到过事件"的话，实现只发一条空批次也能绿。

/// 等到 `SearchDone`，顺手把中间的批次攒起来。
fn drain_search(events: &async_channel::Receiver<wfiles_core::kernel::KernelEvent>) -> Session {
    let mut session = Session::default();
    for _ in 0..300 {
        while let Ok(event) = events.try_recv() {
            match event {
                wfiles_core::kernel::KernelEvent::SearchBatch { hits } => {
                    session.hits.extend(hits);
                }
                wfiles_core::kernel::KernelEvent::SearchDone {
                    visited,
                    matched,
                    truncated,
                    cancelled,
                } => {
                    session.visited = visited;
                    session.matched = matched;
                    session.truncated = truncated;
                    session.cancelled = cancelled;
                    session.done = true;
                }
                _ => {}
            }
        }
        if session.done {
            return session;
        }
        // 搜索在自己的线程上跑：等条件，别用固定更长的 sleep 猜。
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("等不到 SearchDone，收到 {} 条命中", session.hits.len());
}

#[derive(Default)]
struct Session {
    hits: Vec<wfiles_core::contract::SearchHit>,
    visited: usize,
    matched: usize,
    truncated: bool,
    cancelled: bool,
    done: bool,
}

fn search_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("临时目录");
    fs::write(dir.path().join("alpha.txt"), b"aa").expect("写文件");
    fs::write(dir.path().join("beta.txt"), b"bb").expect("写文件");
    fs::create_dir(dir.path().join("sub")).expect("建子目录");
    fs::write(dir.path().join("sub").join("alpha-two.txt"), b"cc").expect("写子目录里的文件");
    dir
}

/// 名字搜索：子目录里的也要搜到，统计要对得上。
#[test]
fn a_new_frontend_searches_by_streaming_hits() {
    let dir = search_fixture();
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let events = core.subscribe();

    // 回执给的是"清单没变"（搜索不动眼前这个目录），结果走事件流。
    let before = listing(core.apply(Command::Refresh));
    let after = listing(core.apply(Command::Search {
        root: dir.path().to_path_buf(),
        query: "alpha".to_owned(),
    }));
    assert_eq!(after, before, "搜索不该改动眼前这份清单");

    let session = drain_search(&events);
    assert_eq!(session.matched, 2, "alpha 命中两个文件");
    assert_eq!(session.hits.len(), 2, "批次里也该正好两条");
    assert!(!session.cancelled && !session.truncated);
    assert!(
        session.visited >= 4,
        "visited 该数过目录树里的条目（alpha/beta/sub/alpha-two），实际 {}",
        session.visited
    );
    let names: Vec<_> = session.hits.iter().map(|hit| hit.name.as_str()).collect();
    assert!(names.contains(&"alpha.txt"), "{names:?}");
    assert!(
        names.contains(&"alpha-two.txt"),
        "子目录里的也得搜到：{names:?}"
    );
    assert!(
        session.hits.iter().all(|hit| hit.line.is_none()),
        "没写 content: 就不该去读文件内容"
    );
}

/// 内容搜索：命中要带回行号与那一行的原文（大小写按文件里原来的）。
#[test]
fn content_hits_come_back_with_the_line() {
    let dir = tempfile::tempdir().expect("临时目录");
    fs::write(
        dir.path().join("code.txt"),
        b"first line\nlet Needle = 1;\nlast line\n",
    )
    .expect("写文件");

    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    listing(core.apply(Command::Search {
        root: dir.path().to_path_buf(),
        query: "content:needle".to_owned(),
    }));

    let session = drain_search(&events);
    assert_eq!(session.matched, 1, "只有 code.txt 含 needle");
    let hit = session.hits.first().expect("该有一条命中");
    assert_eq!(hit.name, "code.txt");
    assert_eq!(
        hit.line
            .as_ref()
            .map(|(number, text)| (*number, text.as_str())),
        Some((2, "let Needle = 1;")),
        "要带行号（从 1 起）和原文（保留原大小写）"
    );
}

/// 叫得停：`CancelSearch` 之后不该还把整棵树走完。
///
/// 判据为什么是"或"：搜索在自己的线程上跑，叫停可能落在它已经走完之后
/// （几百个文件几十毫秒就完了）。两种结果都说明取消没帮倒忙；**只有把取消
/// 做成空操作**才会出现"一棵树走完 + cancelled=false"这一格。
#[test]
fn a_search_can_be_called_off() {
    let dir = tempfile::tempdir().expect("临时目录");
    let files = 800;
    for index in 0..files {
        fs::write(dir.path().join(format!("f{index}.txt")), b"x").expect("写文件");
    }

    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let events = core.subscribe();
    listing(core.apply(Command::Search {
        root: dir.path().to_path_buf(),
        query: "*".to_owned(),
    }));
    listing(core.apply(Command::CancelSearch));

    let session = drain_search(&events);
    assert!(
        session.cancelled || session.visited < files,
        "叫停之后不该还是整棵树都走完：visited={} cancelled={}",
        session.visited,
        session.cancelled
    );

    // 取消是幂等的：没在搜的时候再叫一次也该接受，不该报错。
    listing(core.apply(Command::CancelSearch));
}

/// 看不懂的查询要退回一句能读的话，别起一趟什么都不会命中的搜索。
#[test]
fn a_broken_query_is_rejected_with_a_readable_message() {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);
    let events = core.subscribe();

    match core.apply(Command::Search {
        root: dir.path().to_path_buf(),
        query: "content:".to_owned(),
    }) {
        Event::Failure { message } => assert!(!message.is_empty(), "拒绝要给一句能读的话"),
        other => panic!("content: 后面没词，不该被接受：{other:?}"),
    }
    // 被拒绝的命令不该留下后台还在跑的东西。
    while events.try_recv().is_ok() {}
}

/// 「下一步会撤销什么」也是只读查询：新前端不必去订阅事件流、
/// 更不该自己拼一句描述（GUI 的撤销按钮早就写着「撤销 XXX」）。
///
/// 这条链顺带钉住撤销栈的后进先出与 `note_renamed`：改名之后更早那条
/// 「新建」会被指向新名字，撤掉改名后必须指回旧名字，否则标签会撒谎。
#[test]
fn a_new_frontend_can_ask_what_undo_would_undo() {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut core = Core::new(dir.path()).with_locale(Locale::ZhHans);

    // 空栈：既不能撤，也不该编出一句描述。
    assert!(!core.can_undo());
    assert!(core.undo_label().is_none(), "没东西可撤时不该有标签");

    listing(core.apply(Command::CreateFolder {
        parent: dir.path().to_path_buf(),
        name: "sub".to_owned(),
    }));
    assert_eq!(
        core.undo_label().map(|msg| msg.to_string()),
        Some("新建“sub”".to_owned()),
        "标签该由 core 的 HistoryEntry::describe 出，不是前端拼的"
    );

    // 再叠一步改名：栈顶变了，标签跟着变。
    listing(core.apply(Command::Rename {
        from: dir.path().join("sub"),
        to: "renamed".to_owned(),
    }));
    assert_eq!(
        core.undo_label().map(|msg| msg.to_string()),
        Some("重命名“sub”为“renamed”".to_owned()),
        "后进先出：标签该说撤的是改名"
    );

    listing(core.apply(Command::Undo));
    assert_eq!(
        core.undo_label().map(|msg| msg.to_string()),
        Some("新建“sub”".to_owned()),
        "撤掉改名后，更早那条「新建」得指回旧名字"
    );

    listing(core.apply(Command::Undo));
    assert!(core.undo_label().is_none(), "栈空了标签也该消失");
    assert!(!core.can_undo());
}
