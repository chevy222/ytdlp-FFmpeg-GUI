//! 本地时间格式化（无 chrono 依赖）。
//!
//! std 只提供 UTC epoch 秒，这里提供"epoch 秒 + 显式时区偏移"的纯函数
//! （可单测）；偏移量本模块自行探测（Windows 读注册表 `ActiveTimeBias`，
//! 其余平台按 UTC）。此前各处直接用 UTC 计算日期，东八区 0:00–8:00 会取到
//! "昨天"，合并默认名 / `日期-标题` 模板 / `updated_at` 均受影响。

/// 当前 epoch 秒。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// days since 1970-01-01 -> (年, 月, 日)。Howard Hinnant 公历算法。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 拆分 epoch 秒为 (年, 月, 日, 时, 分, 秒)。
fn split(secs: u64, offset_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let total = secs as i64 + offset_secs;
    let days = total.div_euclid(86_400);
    let rem = total.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// `YYYYMMDD` 日期戳（合并默认名 `合并_<YYYYMMDD>`）。
pub fn date_stamp(secs: u64, offset_secs: i64) -> String {
    let (y, mo, d, ..) = split(secs, offset_secs);
    format!("{:04}{:02}{:02}", y, mo, d)
}

/// `YYYY-MM-DD` 日期（`日期-标题` 文件名模板前缀）。
pub fn date_str(secs: u64, offset_secs: i64) -> String {
    let (y, mo, d, ..) = split(secs, offset_secs);
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

/// `YYYY-MM-DD HH:MM:SS` 时间戳（条目 `updated_at`）。
pub fn datetime_str(secs: u64, offset_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = split(secs, offset_secs);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s)
}

/// 本地时区偏移秒（东八区 = 28800）。
///
/// - Windows：注册表 `ActiveTimeBias`（当前生效偏差，含夏令时；
///   bias = UTC − 本地，单位分钟）。经 `reg query` 读取——core 层保持
///   平台无关，不引入 Win32 依赖；
/// - 其余平台：返回 0（UTC）。本项目生产环境为 Windows，测试按 UTC 断言。
///
/// **缓存 + TTL（5 分钟）**：`ActiveTimeBias` 会随夏令时切换变化，用 `OnceLock`
/// 一次探测永久缓存，会让切换后所有时间戳（`updated_at`、`日期-标题`、
/// 日志文件名）整整差一小时。TTL 到期重探；探测失败一律回退 0，不阻塞调用方。
pub fn local_offset_secs() -> i64 {
    const TTL: std::time::Duration = std::time::Duration::from_secs(300);
    // LazyLock + Mutex：偏移会随夏令时变化，不能一次探测永久缓存（见上）
    static CACHE: std::sync::LazyLock<parking_lot::Mutex<Option<(std::time::Instant, i64)>>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(None));
    let mut cache = CACHE.lock();
    if let Some((at, v)) = *cache {
        if at.elapsed() < TTL {
            return v;
        }
    }
    // 锁内探测（约十几毫秒、5 分钟一次）：避免并发调用各起一个 reg query
    let v = detect_offset();
    *cache = Some((std::time::Instant::now(), v));
    v
}

/// 启动时预热时区偏移：`MediaItem::new`（纯数据构造）会用到它，提前探好就不会
/// 让"创建条目"这条路径隐式产生一个 `reg query` 子进程。
pub fn warm_up() {
    let _ = local_offset_secs();
}

#[cfg(windows)]
fn detect_offset() -> i64 {
    let mut cmd = std::process::Command::new("reg");
    cmd.args([
        "query",
        r"HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation",
        "/v",
        "ActiveTimeBias",
    ]);
    crate::exec::hide_console(&mut cmd);
    let out = match cmd.output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // 输出行形如：`    ActiveTimeBias    REG_DWORD    0xfffffe20`
    text.lines()
        .find(|l| l.contains("ActiveTimeBias"))
        .and_then(|l| l.split_whitespace().find(|t| t.starts_with("0x")))
        .and_then(|t| u32::from_str_radix(t.trim_start_matches("0x"), 16).ok())
        .map(|bias| (-(bias as i32)) as i64 * 60)
        .unwrap_or(0)
}

#[cfg(not(windows))]
fn detect_offset() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(datetime_str(0, 0), "1970-01-01 00:00:00");
        assert_eq!(date_stamp(0, 0), "19700101");
    }

    #[test]
    fn offset_shifts_day() {
        // 1_789_675_200 = 2026-09-17 20:00:00 UTC，+8h 后是 09-18 凌晨
        assert_eq!(datetime_str(1_789_675_200, 0), "2026-09-17 20:00:00");
        assert_eq!(datetime_str(1_789_675_200, 8 * 3600), "2026-09-18 04:00:00");
        assert_eq!(date_stamp(1_789_675_200, 8 * 3600), "20260918");
    }

    #[test]
    fn negative_offset_crosses_month() {
        // 1_772_325_000 = 2026-03-01 00:30:00 UTC，-1h 回到 02-28（2026 非闰年）
        let secs = 1_772_325_000;
        assert_eq!(datetime_str(secs, 0), "2026-03-01 00:30:00");
        assert_eq!(datetime_str(secs, -3600), "2026-02-28 23:30:00");
    }

    #[test]
    fn leap_year_feb_29() {
        // 2024-02-29 12:00:00 UTC
        assert_eq!(datetime_str(1_709_208_000, 0), "2024-02-29 12:00:00");
    }
}
