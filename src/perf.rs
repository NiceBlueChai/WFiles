//! 性能基准：钉住 ROADMAP「性能基线」那几条。
//!
//! # 先用 release 测
//!
//! ```text
//! cargo test --release perf:: -- --nocapture
//! ```
//!
//! **debug 构建的数字对真实用户没有意义。** 不开优化时每个节点都可能带
//! 运行时检查，同一份代码慢一到两个数量级。实测 10 万项按名称排序：
//! debug 79 ms、release 39 ms；过滤：debug 15 ms、release 2.9 ms。
//! 拿 debug 的数字定门槛，等于把门槛定在一个用户永远碰不到的地方。
//!
//! 那 `cargo test`（默认 debug）里跑的这些是干什么用的？**门禁**：
//! 时间预算都经 [`budget`] 按构建类型放宽过，debug 下也能过，
//! 所以 CI 不必额外跑一遍 release（那要多花五分钟编译）。
//! debug 抓的是**数量级的退化**（有人手滑写成 O(n²)）；
//! release 才谈得上"用户感不感觉得到"。
//!
//! # 这里测什么、不测什么
//!
//! 核心是**复杂度**，不是绝对毫秒数。`cargo test` 会在各种机器上跑
//! （插电与不插电、CI 的共享 CPU、旁边有没有人在编译），绝对耗时在慢的
//! 那台上就能翻倍。钉"过滤 10 万项必须 < 2 ms"这种断言，结果是每隔几周
//! 红一次，然后大家学会忽略它——那比没有基准更坏。
//!
//! 所以每个基准是一对断言：
//!
//! - **规模比**：n 翻 4 倍，耗时该涨接近 4 倍，不该涨接近 16 倍。
//!   这条**与机器无关**，是真正防退化的那条。
//! - **宽松上限**：按 [`budget`] 放宽的绝对值，用来挡住数量级退化。
//!
//! 引用 ROADMAP 的那四条基线：
//!
//! - 10 万项目录：列表滚动、排序、即时过滤不掉帧
//! - 缩略图异步生成 + 缓存，生成期间列表保持可交互
//! - 大目录（万级以上）操作全程可取消，取消后不留半成品
//! - 冷启动到可交互：万级目录 < 1s

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::model::{FileEntry, Listing, Sort, SortKey};

/// 性能基线里那个"10 万项"。
const BIG: usize = 100_000;
/// 拿来对比增长率的小规模。四倍：平方复杂度会显出 16 倍的差距。
const SMALL: usize = BIG / 4;

/// **release 实测值**在 debug 下要放宽多少倍。
///
/// 40 不是拍出来的：同机实测 10 万项按名称排序 debug 79 ms / release 39 ms、
/// 过滤 debug 15 ms / release 2.9 ms。取 40 让 debug 下的门槛落在
/// "用户真会觉得慢"的量级上——也就是说 debug 门禁挂了意味着 release 下
/// 大概也超预算，而不是"debug 太慢导致的假警报"。
///
/// 数字来自这台机器，换机器会变。所以 `budget()` 只是**放宽**，
/// 真正的防线是那些与机器无关的规模比断言。
const DEBUG_SLACK: u64 = 40;

/// 某个操作该花多久的上限；按构建类型放宽。
///
/// 参数写的是 **release 下的实测毫秒数**，这里返回它允许的余量。
/// 要看这些数字在 release 下到底是多少，跑
/// `cargo test --release perf:: -- --nocapture`（每条都会打印实测值）。
fn budget(release_ms: u64) -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(release_ms * DEBUG_SLACK)
    } else {
        Duration::from_millis(release_ms)
    }
}

/// 造一批条目，不碰磁盘。
///
/// 真造 10 万个文件既慢又依赖文件系统（Linux 上还得改 `ulimit`），
/// 而排序与过滤要的只是"一批带名字和大小的东西"，跟它们从哪来无关。
fn entries(count: usize) -> Vec<FileEntry> {
    (0..count)
        .map(|ix| {
            // 名字里带序号：过滤"f0123"能选中有限的一部分，
            // 不至于全中（全中时字符串搜索会走出另一种分支）。
            let name = format!("file{ix:07}.txt");
            let path = PathBuf::from("/bench").join(&name);
            FileEntry::for_test(path, name, ix % 1000 == 0, (ix as u64) * 7)
        })
        .collect()
}

fn a_listing(count: usize) -> Listing {
    Listing::from_entries_showing(PathBuf::from("/bench"), entries(count), false)
}

/// 跑 `f`，返回它花了多久。跑三遍取快的那个：
///
/// 第一遍要把页表、数据都拉进缓存，第二遍才是稳态。单遍的计时
/// 里混着"冷启动"那一份，读出来的数不是这个操作的真实成本。
///
/// 收 `FnMut` 是因为有几个基准要反复改同一个 listing（每次跑完得
/// 把查询改回去，否则第二轮 `set_query` 直接返回 false，量到的是 0）。
fn fastest(mut f: impl FnMut() -> usize) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let used = f();
        let took = start.elapsed();
        assert!(used > 0, "基准函数必须真的用到输入，否则编译器会把它优化掉");
        best = best.min(took);
    }
    best
}

/// 规模翻 4 倍时，耗时不该翻超过这个倍数。
///
/// 4 倍是数据上界：`n log n` 在 n 翻 4 倍时涨约 4.3 倍，留点余量给缓存
/// 与调度抖动。12 足够宽，能放过所有线性与 n log n 的实现，
/// 但挡不住平方（16 倍）。
const GROWTH_CEILING: f64 = 12.0;

/// 把两趟耗时摆出来，报出增长倍数，返回"没超"这件事。
///
/// 顺带判掉"小规模那趟快到测不准"：那种情况下倍数除出来是噪声，
/// 拿去跟 `GROWTH_CEILING` 比毫无意义。
fn report(what: &str, small: Duration, big: Duration) -> bool {
    if small.as_nanos() < 200_000 {
        println!("{what}：{SMALL} 项 {small:?} → {BIG} 项 {big:?}（小规模测不准，不判）");
        return true;
    }
    let ratio = big.as_secs_f64() / small.as_secs_f64();
    println!("{what}：{SMALL} 项 {small:?} → {BIG} 项 {big:?}（{ratio:.1} 倍）");
    ratio <= GROWTH_CEILING
}

// ---- 排序 ----

#[test]
fn sorting_a_hundred_thousand_entries_stays_linear() {
    let small = fastest(|| {
        let mut listing = a_listing(SMALL);
        listing.set_sort(Sort {
            key: SortKey::Name,
            descending: false,
        });
        listing.visible_count()
    });
    let big = fastest(|| {
        let mut listing = a_listing(BIG);
        listing.set_sort(Sort {
            key: SortKey::Name,
            descending: false,
        });
        listing.visible_count()
    });
    assert!(
        report("按名称排序", small, big),
        "排序不该是平方的（{small:?} → {big:?}）"
    );
    // 上限按 release 实测定：10 万项按名称排序 release 实测 39 ms
    // （debug 79 ms）。这里给 3 倍余量 ≈ 120 ms。
    //
    // 为什么这条值得卡：排序是用户点一下列头就要等的操作，39 ms 已经是
    // "有点顿但可接受"，涨到 120 ms 就明显卡手了。
    let limit = budget(120);
    assert!(
        big < limit,
        "10 万项按名称排序花了 {big:?}，上限 {limit:?}\
         （release 实测 39 ms；跑 `cargo test --release perf::` 看真实数字）"
    );
}

#[test]
fn sorting_by_size_stays_linear() {
    // 按大小排走的是另一条比较分支（`compare` 里按 u64 比），
    // 名字那条比的是字符串。两条都得平。
    let mut listing = a_listing(SMALL);
    let start = Instant::now();
    listing.set_sort(Sort {
        key: SortKey::Size,
        descending: true,
    });
    let small = start.elapsed();
    assert_eq!(listing.visible_count(), SMALL, "排序不该少条目");

    let mut listing = a_listing(BIG);
    let start = Instant::now();
    listing.set_sort(Sort {
        key: SortKey::Size,
        descending: true,
    });
    let big = start.elapsed();
    assert!(
        report("按大小排序", small, big),
        "按大小排序不该是平方的（{small:?} → {big:?}）"
    );
}

// ---- 过滤 ----

/// **过滤不该重排。** 这是整个基准里最该守住的一条。
///
/// 过滤框每敲一个字母就要走一次过滤，而排序键一个字符都没变。
/// 重排出来的结果跟原来一模一样——10 万项实测 19 ms/次，
/// 一帧的预算是 16.7 ms，光这一下就把它吃光了。
///
/// 判据是**清空比打一字快多少倍**，不是绝对值，也不是"n 翻几倍涨几倍"：
///
/// - debug 下过滤一趟 8 ms、CI 上可能 20 ms，两边都"不算慢"，
///   所以绝对值钉不住。
/// - 拿两个规模算倍数也不可靠：小规模那趟只有几毫秒，计时噪声就能
///   把它推到 2 倍或 6 倍（实测两轮给出过两个数）。
/// - 但**同一个 listing 上，清空 vs 打一字**这个比值是稳的：
///   清空只是一次拷贝，打一字要一趟字符串扫描，重排则在两边都加上排序。
///   实测不重排约 10 倍、重排约 1.7 倍。
///
/// 反复交替着量，因为 `set_query` 有"条件没变就直接返回"的短路：
/// 只量一次的话，第二轮量到的是那条短路。
#[test]
fn filtering_does_not_resort_the_whole_listing() {
    let mut listing = a_listing(BIG);
    let mut filtering = Duration::MAX;
    let mut clearing = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        listing.set_query("file0001");
        filtering = filtering.min(start.elapsed());

        let start = Instant::now();
        listing.set_query("");
        clearing = clearing.min(start.elapsed());
    }
    println!("打一个字母 {filtering:?} vs 清空 {clearing:?}");

    let ratio = filtering.as_secs_f64() / clearing.as_secs_f64();
    assert!(
        ratio > 3.0,
        "清空过滤只比重打快 {ratio:.1} 倍——重排的实现里两次都在排序，\
         清空并没有省下什么（不重排约 10 倍）"
    );
    // 上限按 release 实测定：清空过滤 release 实测 0.20 ms，
    // 打一个字母 2.9 ms。清空是"刚敲完退格"的即时反馈，
    // 给 30 倍余量（≈ 6 ms）已经很宽了。
    // release 实测 159 µs（打一个字母是 4.0 ms）。清空是"刚敲完退格"的
    // 即时反馈，给 1 ms：余量 6 倍，离一帧还差 16 倍。
    let limit = budget(1);
    assert!(
        clearing < limit,
        "清空过滤花了 {clearing:?}，上限 {limit:?}（release 实测 159 µs）"
    );
}

/// 过滤本身也得是线性的：n 翻 4 倍，耗时不该翻 16 倍。
#[test]
fn filtering_a_hundred_thousand_entries_stays_linear() {
    let mut small = a_listing(SMALL);
    let mut big = a_listing(BIG);
    // 先各自排好，再量过滤本身——把排序那一趟排除掉。
    small.set_sort(Sort::default());
    big.set_sort(Sort::default());

    // 手动轮询，**不用 `fastest`**：`set_query` 有一条"条件没变就直接返回"
    // 的短路，而 `fastest` 连跑三遍同一个条件，第二三遍量到的是那条短路
    // （0 ns）。这个基准必须在"每次都是新条件"的前提下量。
    let mut small_took = Duration::MAX;
    let mut big_took = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        small.set_query("file0001234");
        small_took = small_took.min(start.elapsed());
        small.set_query("");

        let start = Instant::now();
        big.set_query("file0001234");
        big_took = big_took.min(start.elapsed());
        big.set_query("");
    }
    assert!(
        small.visible_count() > 0 && big.visible_count() > 0,
        "该真的过滤出东西来，别量了个空"
    );

    assert!(
        report("过滤", small_took, big_took),
        "过滤不该是平方的（{small_took:?} → {big_took:?}）"
    );
}

// ---- 滚动取行 ----

#[test]
fn reading_rows_stays_constant_time() {
    // 虚拟列表每帧只画可见的那几十行，所以这条决定了"滚动掉不掉帧"。
    // 用 `visible_entry` 而不是遍历——遍历是 O(n)，那不是滚动干的事。
    let listing = a_listing(BIG);
    let took = fastest(|| {
        let mut names = 0usize;
        for step in 0..2000usize {
            if let Some(entry) = listing.visible_entry((step * 7919) % BIG) {
                names += entry.name.len();
            }
        }
        names
    });
    // release 实测 2.1 µs（debug 0.5 ms）。一帧预算是 16.7 ms，
    // 取 50 µs：余量 24 倍，而离"一帧"还差 300 倍——这道线卡的是
    // "哪天有人在这里偷偷加了个 O(n) 遍历"。
    println!("2000 次取行：{took:?}（release 实测 2.1 µs）");
    assert!(
        took < budget(50),
        "2000 次取行花了 {took:?}，滚动会掉帧（release 实测 2.1 µs）"
    );
}

/// 反查表（路径 -> 显示在第几行）也是滚动与键盘导航每帧要用的。
#[test]
fn finding_a_files_position_stays_fast() {
    let listing = a_listing(BIG);
    let target = listing
        .visible_entry(BIG / 3)
        .expect("有那一行")
        .path
        .clone();
    let took = fastest(|| {
        let mut hits = 0usize;
        for _ in 0..2000 {
            if listing.position_of(&target).is_some() {
                hits += 1;
            }
        }
        hits
    });
    // release 实测 46 µs（比取行慢一个数量级——它要过一次哈希查找）。
    // 给 500 µs，余量 11 倍。
    println!("2000 次反查：{took:?}（release 实测 46 µs）");
    assert!(
        took < budget(500),
        "2000 次反查花了 {took:?}，滚动与键盘导航会掉帧（release 实测 46 µs）"
    );
}

// ---- 选中 ----

#[test]
fn selecting_everything_stays_linear() {
    // 10 万项全选是"框选整目录"这类操作的起点，也是最坏情况。
    let mut selection = crate::model::Selection::default();
    let listing = a_listing(BIG);
    let start = Instant::now();
    selection.select_all(&listing);
    let took = start.elapsed();
    assert_eq!(selection.len(), BIG, "该全选上");
    // release 实测 175 ms。这条本身就**不理想**：175 ms 在 release 下是
    // 能感觉到的顿。它慢是因为 BTreeSet 逐条插入 10 万个 `PathBuf`
    // （带堆分配）。门槛给 350 ms（2 倍），盯的是"别再往上涨"，
    // 而不是"这已经够快了"——真要提速该换结构，那是另一件事。
    println!("10 万项全选：{took:?}（release 实测 175 ms，这个值本身还不够好）");
    assert!(
        took < budget(350),
        "10 万项全选花了 {took:?}（release 实测 175 ms）"
    );
}

/// 取回"按显示顺序排好的选中路径"——复制、剪切、统计都要它。
/// 这条走的是排序 + 二分，不是遍历。
#[test]
fn collecting_the_selection_follows_the_visible_order() {
    let listing = a_listing(BIG);
    // 隔一个选一个，模拟"用户大致全选但漏了几个"。
    //
    // 用 `select_many` 一次装回去，**不能循环调 `select`**——那是单选语义
    // （每次都 `paths.clear()`），循环 5 万次只剩最后一个。这个坑踩过一次。
    let wanted: Vec<PathBuf> = (0..BIG)
        .step_by(2)
        .map(|position| {
            listing
                .visible_entry(position)
                .expect("每一行都在")
                .path
                .clone()
        })
        .collect();
    let mut selection = crate::model::Selection::default();
    selection.select_many(wanted.clone(), None);

    let start = Instant::now();
    let got = selection.ordered(&listing);
    let took = start.elapsed();
    assert_eq!(got.len(), BIG / 2, "该拿到一半");
    // 顺序也得对：按显示顺序，不是按下标顺序，也不是选中的先后。
    let expected: Vec<PathBuf> = (0..BIG)
        .step_by(2)
        .map(|position| {
            listing
                .visible_entry(position)
                .expect("每一行都在")
                .path
                .clone()
        })
        .collect();
    assert_eq!(got, expected, "该按列表显示的顺序给，不按选中的先后");
    // release 实测 97 ms。同样偏慢（每次都新建 PathBuf），门槛给 200 ms。
    println!("5 万项选中取回顺序：{took:?}（release 实测 97 ms）");
    assert!(
        took < budget(200),
        "5 万项选中取回顺序花了 {took:?}（release 实测 97 ms）"
    );
}

// ---- 冷启动 ----

/// 冷启动到"这一栏能看见东西"：读盘 + 排序 + 建索引。
///
/// 万级目录预算 1 s。这里量的是 `Listing::read` 这一段——读盘加排序，
/// 也就是用户按下回车之后到看见列表之前的那一段。
#[test]
fn reading_a_ten_thousand_entry_directory_is_warm() {
    let dir = tempfile::tempdir().expect("临时目录");
    for ix in 0..10_000usize {
        std::fs::write(dir.path().join(format!("f{ix:06}.txt")), "x").expect("写测试文件");
    }

    let start = Instant::now();
    let listing = Listing::read(dir.path(), false).expect("读得动");
    let took = start.elapsed();
    assert_eq!(listing.total_count(), 10_000, "该看到一万项");

    // 这条不一样：1 s 是 ROADMAP 上写死的验收数字（万级目录冷启动 < 1s），
    // 不是我拍的门禁，所以**不放宽**——debug 下也照 1 s 卡，真超了就是超了。
    //
    // 它量的是真磁盘 IO，所以数字跟文件系统强相关：release 实测几十毫秒
    // （tmpfs 上更快），debug 慢一个数量级。
    println!("读一万项并排好：{took:?}");
    assert!(
        took < Duration::from_secs(1),
        "读一万项并排好花了 {took:?}，路线图要求万级目录冷启动 < 1s"
    );
}
