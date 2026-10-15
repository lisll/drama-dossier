//! 防时钟倒退。
//!
//! 离线时只能相信本机时钟：把系统时间往回调，就能无限拖长离线宽限期。
//! 所以记住「见过的最晚时间」（high water），和授权凭证存在同一条钥匙串记录里——
//! 删掉这个时间，凭证也一起没了，只能重新联网。
//!
//! 规则：
//! - 离线检查时，「现在」比「见过的最晚时间」或凭证的签发时间 `iat` 早超过 [`ROLLBACK_TOLERANCE_SECS`]，
//!   就判定时钟倒退，离线授权立即失效；
//! - 容差留给自动对时的正常回拨；
//! - 联网刷新成功后，最晚时间重置为「现在」和 `iat` 中较晚的那个
//!   （[`reset_after_online`]）。所以把时间调到未来再调回来，联网一次就能恢复。
//!
//! 边界：挡得住「往回调时间」，挡不住「改掉检查代码再自己编译」。

/// 允许的回拨幅度：10 分钟。
pub const ROLLBACK_TOLERANCE_SECS: u64 = 10 * 60;

/// 最晚时间至少前进这么多秒才写一次钥匙串，避免频繁写。
pub const PERSIST_STEP_SECS: u64 = 60;

/// 一次时钟检查的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockVerdict {
    /// 时钟正常；带上更新后的最晚时间。
    Ok { high_water: u64 },
    /// 时钟倒退；`behind_secs` 是比参照时间早了多少秒。
    Rollback { behind_secs: u64 },
}

/// 离线检查：参照时间取「见过的最晚时间」和凭证签发时间中较晚的那个。
pub fn check(now: u64, high_water: u64, issued_at: u64) -> ClockVerdict {
    let floor = high_water.max(issued_at);
    if now.saturating_add(ROLLBACK_TOLERANCE_SECS) < floor {
        return ClockVerdict::Rollback {
            behind_secs: floor - now,
        };
    }
    ClockVerdict::Ok {
        high_water: floor.max(now),
    }
}

/// 联网刷新成功后的最晚时间：「现在」和新凭证的签发时间中较晚的那个。
pub fn reset_after_online(now: u64, issued_at: u64) -> u64 {
    now.max(issued_at)
}

/// 最晚时间前进超过 [`PERSIST_STEP_SECS`] 才需要写回钥匙串。
pub fn should_persist(stored: u64, current: u64) -> bool {
    current >= stored.saturating_add(PERSIST_STEP_SECS)
}
