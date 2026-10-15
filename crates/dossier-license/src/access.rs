//! 使用权判定：把授权凭证、时钟、联网结果合成三档。
//!
//! - 联网（[`Tier::Online`]）：全部功能可用，包括必须联网的付费步骤；
//! - 离线（[`Tier::Offline`]）：免费的本地功能可用，截止到凭证 `exp` 和使用权 `ent_until` 中较早的那个；
//! - 锁定（[`Tier::Locked`]）：带原因，界面按原因给出下一步。
//!
//! 凭证有效只说明「这台电脑属于这个账号」，能不能用还要看使用权：
//! 联网时以服务端返回的 `entitlement.active` 为准；断网时用凭证里的 `ent` / `ent_until` 和本机时间判断。
//! 月卡用户的凭证可能还没过期、使用权已经到期，这种情况同样锁定并提示续期。

use serde::Serialize;

use crate::clock::{self, ClockVerdict};
use crate::token::{EntitlementKind, License, TokenError};

/// 使用权到期前多久开始提示续期：3 天。
pub const RENEW_WARNING_SECS: u64 = 3 * 24 * 3600;

/// 锁定原因。`code()` 是给前端分支用的机器码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockReason {
    /// 没有使用权（没兑换过卡密）。
    NoEntitlement,
    /// 使用权已到期（月卡过期）。
    EntitlementExpired,
    /// 断网太久，凭证过期了。
    GraceExpired,
    /// 系统时间被往回调了。
    ClockRollback,
    /// 没有登录。
    NotLoggedIn,
    /// 这台电脑已被解绑，或者会话已失效。
    DeviceUnbound,
    /// 本地凭证无效（签名不对、不属于这台电脑等），需要联网换一张。
    LicenseInvalid,
    /// 已登录，但这台电脑还没绑定（名额满了，等用户先解绑一台）。
    NeedsActivation,
    /// 系统钥匙串不可用，登录状态存不下来。
    StorageUnavailable,
    /// 安装包配置有误（公钥、服务端地址、读不到机器码）。
    ConfigError,
}

impl LockReason {
    pub fn code(self) -> &'static str {
        match self {
            LockReason::NoEntitlement => "no_entitlement",
            LockReason::EntitlementExpired => "entitlement_expired",
            LockReason::GraceExpired => "grace_expired",
            LockReason::ClockRollback => "clock_rollback",
            LockReason::NotLoggedIn => "not_logged_in",
            LockReason::DeviceUnbound => "device_unbound",
            LockReason::LicenseInvalid => "license_invalid",
            LockReason::NeedsActivation => "needs_activation",
            LockReason::StorageUnavailable => "storage_unavailable",
            LockReason::ConfigError => "config_error",
        }
    }

    /// 给用户看的说明和下一步。
    pub fn message(self) -> &'static str {
        match self {
            LockReason::NoEntitlement => "这个账号还没有使用权，请先兑换卡密",
            LockReason::EntitlementExpired => "使用权已到期，请兑换新的卡密续期",
            LockReason::GraceExpired => "已经断网太久，离线使用期已过，请联网后重试",
            LockReason::ClockRollback => "系统时间比之前见过的时间早，请校准系统时间后联网",
            LockReason::NotLoggedIn => "请先登录",
            LockReason::DeviceUnbound => "这台电脑可能已在别处被解绑，请重新登录",
            LockReason::LicenseInvalid => "本机的授权凭证无效或缺失，请联网后重试",
            LockReason::NeedsActivation => "这个账号绑定的电脑已满，请先解绑一台，再绑定这台电脑",
            LockReason::StorageUnavailable => "系统钥匙串不可用，登录状态保存不下来",
            LockReason::ConfigError => "安装包配置有误，请重新下载安装包",
        }
    }
}

/// 三档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Online,
    /// 离线可用，到 `until`（Unix 秒）为止。
    Offline {
        until: u64,
    },
    Locked(LockReason),
}

impl Tier {
    /// 免费的本地功能能不能用。
    pub fn allows_local(self) -> bool {
        matches!(self, Tier::Online | Tier::Offline { .. })
    }

    /// 必须联网的付费步骤能不能用。
    pub fn allows_paid(self) -> bool {
        matches!(self, Tier::Online)
    }

    pub fn code(self) -> &'static str {
        match self {
            Tier::Online => "online",
            Tier::Offline { .. } => "offline",
            Tier::Locked(_) => "locked",
        }
    }
}

/// 服务端返回的使用权（`/license/activate`、`/license/refresh` 响应里的 `entitlement`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerEntitlement {
    pub kind: EntitlementKind,
    /// 服务端按数据库时间算好的「现在能不能用」。
    pub active: bool,
    /// 使用权到期的 Unix 秒数；永久或没有使用权时为 `None`。
    pub expires_at: Option<u64>,
}

/// 最近一次联网的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// 刚刚联网刷新成功，带上服务端返回的使用权。
    Online(ServerEntitlement),
    /// 连不上服务端，只能靠本地凭证。
    Offline,
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub tier: Tier,
    /// 更新后的「见过的最晚时间」；调用方按 [`clock::should_persist`] 决定要不要写回钥匙串。
    pub high_water: u64,
    /// 月卡 3 天内到期，提示续期。
    pub renew_soon: bool,
}

/// 判定当前档位。`license` 是本地验签（[`crate::token::verify`]）的结果。
pub fn decide(
    license: Result<&License, TokenError>,
    high_water: u64,
    now: u64,
    connectivity: Connectivity,
) -> Access {
    let locked = |reason, high_water| Access {
        tier: Tier::Locked(reason),
        high_water,
        renew_soon: false,
    };
    let license = match license {
        Ok(license) => license,
        Err(TokenError::Expired) => return locked(LockReason::GraceExpired, high_water),
        Err(_) => return locked(LockReason::LicenseInvalid, high_water),
    };

    match connectivity {
        Connectivity::Online(entitlement) => {
            // 联网时以服务端为准：服务端只用数据库时间，改本机时钟影响不到它。
            let high_water = clock::reset_after_online(now, license.issued_at);
            if entitlement.kind == EntitlementKind::None {
                return locked(LockReason::NoEntitlement, high_water);
            }
            // 两次联网之间（最长 1 小时）月卡可能到期：服务端说的到期时间过了也锁，只收紧不放宽。
            let lapsed = entitlement.expires_at.is_some_and(|until| now >= until);
            if !entitlement.active || lapsed {
                return locked(LockReason::EntitlementExpired, high_water);
            }
            Access {
                tier: Tier::Online,
                high_water,
                renew_soon: renew_soon(entitlement.expires_at, now),
            }
        }
        Connectivity::Offline => {
            let high_water = match clock::check(now, high_water, license.issued_at) {
                ClockVerdict::Ok { high_water } => high_water,
                ClockVerdict::Rollback { .. } => {
                    return locked(LockReason::ClockRollback, high_water)
                }
            };
            let until = match license.entitlement {
                EntitlementKind::None => return locked(LockReason::NoEntitlement, high_water),
                EntitlementKind::Lifetime => license.expires_at,
                EntitlementKind::Timed => {
                    // Timed 的凭证一定带 ent_until（验签时已检查）。
                    let ent_until = license.entitlement_until.unwrap_or(0);
                    if now >= ent_until {
                        return locked(LockReason::EntitlementExpired, high_water);
                    }
                    ent_until.min(license.expires_at)
                }
            };
            Access {
                tier: Tier::Offline { until },
                high_water,
                renew_soon: renew_soon(license.entitlement_until, now),
            }
        }
    }
}

fn renew_soon(expires_at: Option<u64>, now: u64) -> bool {
    expires_at.is_some_and(|until| until > now && until - now <= RENEW_WARNING_SECS)
}
