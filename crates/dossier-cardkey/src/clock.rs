//! 时钟防回拨。
//!
//! 客户端记录「见过的最晚时间」`last_seen`。当前时间比 `max(last_seen, iat)`
//! 早出容差以上，就判为回拨并锁定；联网续签成功后由新凭证的 iat 自然恢复。

/// 允许的回拨容差（秒）：吸收 NTP 校时与夏令时之类的小抖动。
pub const ROLLBACK_TOLERANCE_SECS: u64 = 600;
/// `last_seen` 至少前进这么多才值得落盘，避免频繁写文件。
pub const PERSIST_STEP_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockVerdict {
    Ok,
    Rollback,
}

pub fn check_clock(now: u64, last_seen: u64, iat: u64) -> ClockVerdict {
    let floor = last_seen.max(iat);
    if now.saturating_add(ROLLBACK_TOLERANCE_SECS) < floor {
        ClockVerdict::Rollback
    } else {
        ClockVerdict::Ok
    }
}

/// 返回需要写回的新 `last_seen`；不需要写时返回 `None`。只会单调递增。
pub fn next_last_seen(now: u64, last_seen: u64) -> Option<u64> {
    (now >= last_seen.saturating_add(PERSIST_STEP_SECS)).then_some(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_rules() {
        assert_eq!(check_clock(1000, 0, 1000), ClockVerdict::Ok);
        assert_eq!(check_clock(1000 - 600, 0, 1000), ClockVerdict::Ok);
        assert_eq!(check_clock(1000 - 601, 0, 1000), ClockVerdict::Rollback);
        assert_eq!(check_clock(5000, 9000, 1000), ClockVerdict::Rollback);
        assert_eq!(check_clock(8500, 9000, 1000), ClockVerdict::Ok);
    }

    #[test]
    fn last_seen_monotonic_and_throttled() {
        assert_eq!(next_last_seen(100, 0), Some(100));
        assert_eq!(next_last_seen(130, 100), None);
        assert_eq!(next_last_seen(160, 100), Some(160));
        assert_eq!(next_last_seen(50, 100), None);
    }
}
