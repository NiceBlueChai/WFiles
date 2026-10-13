//! 传输限速：一个令牌桶。
//!
//! 为什么要限速：一次几十 GB 的拷贝会把整块磁盘（或整条网线）的带宽吃干，
//! 同一时间想干点别的——开网页、拉代码、开会共享屏幕——全得等着。
//! 限速把"最快"换成"够快且不打扰"。
//!
//! 令牌桶：桶里攒着"此刻还能传多少字节"，按速率往里加，传一次取一次；
//! 取不出来就睡到够为止。**攒有上限**（一秒的量），否则限速之前的空闲
//! 会攒出一大笔，一放开就先冲出去一大截——那不叫限速。
//!
//! 睡也分段睡（见 [`MAX_SLEEP`]）：一次睡几十秒的话，用户按中止要等它睡完。

use std::time::{Duration, Instant};

/// 一次最多睡这么久。
///
/// 手柄（暂停/中止）是在上报进度时才看的，而上报就在睡之前；
/// 睡一小段就回到循环里，中止才跟手。欠着的留到下一趟再睡。
const MAX_SLEEP: Duration = Duration::from_millis(100);

pub struct Limiter {
    /// 每秒多少字节；0 = 不限速。
    rate: u64,
    /// 桶里还剩多少字节。负数表示欠着——上一趟没睡完的那部分。
    tokens: f64,
    /// 上一次记账的时刻。空 = 还没开始，第一趟不白等。
    last: Option<Instant>,
}

impl Limiter {
    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            rate: bytes_per_second,
            tokens: 0.0,
            last: None,
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// 改速率。桶清零：从"不限"切到"限 1 MB/s"时，
    /// 之前攒下的额度不该留着让人爽一下。
    pub fn set_rate(&mut self, bytes_per_second: u64) {
        self.rate = bytes_per_second;
        self.tokens = 0.0;
        self.last = None;
    }

    /// 记下这一笔 `bytes` 字节，返回该睡多久。
    ///
    /// **不真睡**，时间从外面喂进来：这样测试不用 `sleep` 就能验它，
    /// 跑一整套也不占真实时间。
    pub fn take_at(&mut self, bytes: u64, now: Instant) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        let rate = self.rate as f64;
        let elapsed = match self.last {
            Some(last) => now.saturating_duration_since(last),
            // 头一趟桶是满的：不然每个小文件都要先等一下，看着像卡住了。
            None => {
                self.tokens = rate;
                Duration::ZERO
            }
        };
        // 攒有上限：一秒的量。
        self.tokens = (self.tokens + elapsed.as_secs_f64() * rate).min(rate);
        self.tokens -= bytes as f64;
        self.last = Some(now);
        if self.tokens >= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(-self.tokens / rate).min(MAX_SLEEP)
    }

    /// 还欠着没有？（`take` 一次只睡一小段，欠着的部分要接着睡。）
    ///
    /// 一次拷贝一大块（比如一个 1 MB 的数据块）时，欠的可能远不止
    /// 那一小段：不接着睡就等于没限速——大文件一口报上来，睡 100 毫秒了事。
    pub fn still_owing(&self) -> bool {
        self.rate > 0 && self.tokens < 0.0
    }

    /// 记账，并按需要真睡一会儿。返回睡了多久。
    ///
    /// 一次最多睡 [`MAX_SLEEP`]：剩下的账留在桶里，由调用方决定
    /// 什么时候再睡（正好可以顺便看一眼用户按没按中止）。
    pub fn take(&mut self, bytes: u64) -> Duration {
        let wait = self.take_at(bytes, Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        wait
    }
}

/// 面板里能选的那几档。`0` = 不限速。
///
/// 档位而不是"随便填一个数字"：限速要的是"别打扰我"，
/// 让人去心算"8388608 字节每秒是多少"没有意义。
/// 限速档位（字节每秒）。`0` = 不限速，放在第一档。
///
/// **只给数字，不给显示名**：档位是数据，显示名是文案，两件事分开之后
/// 单测能直接查档位而不用先造一个 `App` 上下文（那需要 GPUI 测试运行时）。
/// 中文环境下的显示名是「不限速」——而"不限速"这三个字本身还会在
/// 英文界面里出现（它在面板里跟 "4.8 MB/s" 并列），所以那条要单独查表。
pub fn rates() -> Vec<u64> {
    const MB: u64 = 1_000_000;
    vec![0, MB, 5 * MB, 10 * MB, 20 * MB]
}

/// 一档限速的显示名。速率本身是格式化出来的数字，全平台一样；
/// 只有 `0` 那一档是文字，得查表。
pub fn rate_label(rate: u64, cx: &gpui_kit::App) -> String {
    if rate == 0 {
        crate::i18n::tr(cx, "不限速")
    } else {
        crate::format::speed(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// 测试里的"现在"：从某个固定点往后推，不用真等。
    fn moment(seconds: f64) -> Instant {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        start + Duration::from_secs_f64(seconds)
    }

    #[test]
    fn no_limit_never_waits() {
        let mut limiter = Limiter::new(0);
        assert_eq!(limiter.take_at(u64::MAX / 4, moment(0.0)), Duration::ZERO);
        assert_eq!(limiter.take_at(u64::MAX / 4, moment(0.0)), Duration::ZERO);
    }

    #[test]
    fn the_first_second_is_free() {
        // 一秒的量是白送的：不然每个小文件都要等，看着像卡住了。
        let mut limiter = Limiter::new(1_000);
        assert_eq!(limiter.take_at(1_000, moment(0.0)), Duration::ZERO);
        assert!(limiter.take_at(1, moment(0.0)) > Duration::ZERO);
    }

    #[test]
    fn going_over_the_rate_waits_but_only_a_slice() {
        let mut limiter = Limiter::new(1_000);
        // 欠 1 秒，但一次只睡一小段：中止要跟手。
        assert_eq!(limiter.take_at(2_000, moment(0.0)), MAX_SLEEP);
    }

    #[test]
    fn the_debt_carries_over_to_the_next_round() {
        let mut limiter = Limiter::new(1_000);
        assert_eq!(limiter.take_at(2_000, moment(0.0)), MAX_SLEEP);
        // 还欠着 900 ms，下一趟继续睡，不会因为"睡过一次"就当还完了。
        assert_eq!(limiter.take_at(0, moment(0.0)), MAX_SLEEP);
    }

    #[test]
    fn idle_time_refills_the_bucket() {
        let mut limiter = Limiter::new(1_000);
        assert_eq!(limiter.take_at(1_000, moment(0.0)), Duration::ZERO);
        // 停了 1 秒，桶又攒满：这一趟不用等。
        assert_eq!(limiter.take_at(1_000, moment(1.0)), Duration::ZERO);
    }

    #[test]
    fn the_bucket_caps_at_one_second() {
        let mut limiter = Limiter::new(1_000);
        // 空转 10 秒也只攒 1 秒的量：不然一放开就先冲出去一大截。
        assert_eq!(limiter.take_at(1_000, moment(10.0)), Duration::ZERO);
        assert!(limiter.take_at(1_000, moment(10.0)) > Duration::ZERO);
    }

    #[test]
    fn changing_the_rate_drops_what_was_saved_up() {
        let mut limiter = Limiter::new(0);
        assert_eq!(limiter.take_at(1_000_000, moment(0.0)), Duration::ZERO);
        limiter.set_rate(1_000);
        assert_eq!(limiter.rate(), 1_000);
        // 不限速时"攒下"的额度不能留着：切到 1 KB/s 就得立刻生效。
        assert!(limiter.take_at(1_001, moment(0.0)) > Duration::ZERO);
    }

    #[test]
    fn a_big_chunk_keeps_owing_until_it_is_paid() {
        let mut limiter = Limiter::new(1_000);
        // 一口报 1 MB：一次只能睡一小段，剩下的一直欠着。
        let first = limiter.take_at(1_000_000, moment(0.0));
        assert_eq!(first, MAX_SLEEP);
        assert!(limiter.still_owing(), "欠着的部分得接着睡");
        // 不限速时永远不欠账。
        let mut free = Limiter::new(0);
        free.take_at(1_000_000, moment(0.0));
        assert!(!free.still_owing());
    }

    #[test]
    fn presets_start_with_no_limit() {
        let choices = rates();
        assert_eq!(choices[0], 0, "第一档该是不限速：{choices:?}");
        assert!(choices.len() > 1);
        // 递增：档位排出来的顺序就是用户理解的"越来越快"，
        // 顺序乱了会让人以为限速越设越慢。
        assert!(
            choices.windows(2).all(|pair| pair[0] < pair[1]),
            "限速档位应该递增：{choices:?}"
        );
    }
}
