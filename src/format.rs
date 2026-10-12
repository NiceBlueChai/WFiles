//! 界面上用到的文本格式化。

use std::time::{Duration, SystemTime};

/// 以 1024 进制显示字节数，只保留一位小数；不足 1 KB 时显示整数字节。
pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 传输速度。直接复用 [`file_size`]，再加个 "/s"。
pub fn speed(bytes_per_second: u64) -> String {
    format!("{}/s", file_size(bytes_per_second))
}

/// 剩余时间。秒级精度足够，超过一小时不再细分到秒——
/// "1 小时 23 分 07 秒" 里那个 07 秒没人会看。
pub fn remaining(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0 => "不到 1 秒".to_string(),
        1..=59 => format!("{seconds} 秒"),
        60..=3_599 => format!("{} 分 {:02} 秒", seconds / 60, seconds % 60),
        3_600..=86_399 => format!("{} 小时 {} 分", seconds / 3_600, seconds % 3_600 / 60),
        _ => "超过 1 天".to_string(),
    }
}

/// 相对时间。列表里要一眼看出新旧，精确时间留给详情面板。
pub fn modified(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "未知".to_string();
    };
    let Ok(elapsed) = time.elapsed() else {
        return "刚刚".to_string();
    };

    match elapsed.as_secs() {
        0..=59 => "刚刚".to_string(),
        60..=3_599 => format!("{} 分钟前", elapsed.as_secs() / 60),
        3_600..=86_399 => format!("{} 小时前", elapsed.as_secs() / 3_600),
        86_400..=604_799 => format!("{} 天前", elapsed.as_secs() / 86_400),
        _ => absolute(time),
    }
}

/// 超过一周就用绝对日期，"300 天前" 这种数字没有意义。
fn absolute(time: SystemTime) -> String {
    let unix = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    format!("{year}-{month:02}-{day:02}")
}

/// Howard Hinnant 的 days_from_civil 逆变换，避免为了一个日期引入 chrono。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;

    ((y + i64::from(m <= 2)), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(file_size(0), "0 B");
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(1023), "1023 B");
        assert_eq!(file_size(1024), "1.0 KB");
        assert_eq!(file_size(1536), "1.5 KB");
        assert_eq!(file_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(file_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn speeds_reuse_the_size_units() {
        assert_eq!(speed(512), "512 B/s");
        assert_eq!(speed(1024), "1.0 KB/s");
        assert_eq!(speed(3 * 1024 * 1024), "3.0 MB/s");
    }

    #[test]
    fn remaining_times_get_coarser_as_they_grow() {
        assert_eq!(remaining(Duration::from_millis(400)), "不到 1 秒");
        assert_eq!(remaining(Duration::from_secs(9)), "9 秒");
        assert_eq!(remaining(Duration::from_secs(65)), "1 分 05 秒");
        assert_eq!(remaining(Duration::from_secs(3_725)), "1 小时 2 分");
        assert_eq!(remaining(Duration::from_secs(90_000)), "超过 1 天");
    }

    #[test]
    fn recent_times_are_relative() {
        let now = SystemTime::now();
        assert_eq!(modified(None), "未知");
        assert_eq!(modified(Some(now)), "刚刚");
        assert_eq!(modified(Some(now - Duration::from_secs(90))), "1 分钟前");
        assert_eq!(modified(Some(now - Duration::from_secs(7200))), "2 小时前");
        assert_eq!(
            modified(Some(now - Duration::from_secs(86_400 * 3))),
            "3 天前"
        );
    }

    #[test]
    fn old_times_are_absolute() {
        // 2001-09-09T01:46:40Z
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert_eq!(modified(Some(time)), "2001-09-09");
    }

    #[test]
    fn civil_from_days_handles_the_epoch_and_leap_years() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_323), (2001, 1, 1));
        assert_eq!(civil_from_days(18_262), (2020, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
