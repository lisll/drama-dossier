//! 防时钟倒退和使用权判定（纯函数，不需要签名）。

use dossier_license::clock::{self, check, reset_after_online, should_persist};
use dossier_license::{
    decide, ClockVerdict, Connectivity, EntitlementKind, License, LockReason, ServerEntitlement,
    Tier, TokenError, ROLLBACK_TOLERANCE_SECS,
};

const IAT: u64 = 1_767_225_600;
const EXP: u64 = IAT + 72 * 3600;
const DAY: u64 = 86_400;

fn license(entitlement: EntitlementKind, until: Option<u64>) -> License {
    License {
        kid: "kid".into(),
        user_id: "u-1".into(),
        device_id: "dev".into(),
        entitlement,
        entitlement_until: until,
        issued_at: IAT,
        expires_at: EXP,
    }
}

fn online(kind: EntitlementKind, active: bool, expires_at: Option<u64>) -> Connectivity {
    Connectivity::Online(ServerEntitlement {
        kind,
        active,
        expires_at,
    })
}

// ---------- 时钟 ----------

#[test]
fn rollback_beyond_tolerance_is_detected() {
    let high = IAT + 3600;
    let now = high - ROLLBACK_TOLERANCE_SECS - 1;
    assert_eq!(
        check(now, high, IAT),
        ClockVerdict::Rollback {
            behind_secs: ROLLBACK_TOLERANCE_SECS + 1
        }
    );
}

#[test]
fn rollback_within_tolerance_is_allowed() {
    let high = IAT + 3600;
    // 正好回拨 10 分钟：自动对时的正常幅度，放行，最晚时间不往回走。
    let now = high - ROLLBACK_TOLERANCE_SECS;
    assert_eq!(check(now, high, IAT), ClockVerdict::Ok { high_water: high });
}

#[test]
fn clock_earlier_than_iat_is_rollback() {
    // 最晚时间丢了（0），但现在比凭证签发时间早太多：同样判倒退。
    let now = IAT - ROLLBACK_TOLERANCE_SECS - 1;
    assert!(matches!(check(now, 0, IAT), ClockVerdict::Rollback { .. }));
}

#[test]
fn future_then_back_recovers_after_online() {
    // 先把时间调到未来一天：最晚时间跟着走到未来。
    let future = IAT + DAY;
    let ClockVerdict::Ok { high_water } = check(future, IAT, IAT) else {
        panic!("调到未来不算倒退");
    };
    assert_eq!(high_water, future);
    // 再调回正常时间：离线判倒退。
    let back = IAT + 60;
    assert!(matches!(
        check(back, high_water, IAT),
        ClockVerdict::Rollback { .. }
    ));
    // 联网一次，最晚时间重置为「现在」和新凭证 iat 中较晚的那个，之后恢复正常。
    let reset = reset_after_online(back, IAT);
    assert_eq!(reset, back);
    assert!(matches!(
        check(back + 1, reset, IAT),
        ClockVerdict::Ok { .. }
    ));
}

#[test]
fn persist_only_after_one_minute() {
    assert!(!should_persist(IAT, IAT + clock::PERSIST_STEP_SECS - 1));
    assert!(should_persist(IAT, IAT + clock::PERSIST_STEP_SECS));
}

// ---------- 使用权：离线 ----------

#[test]
fn offline_timed_around_ent_until() {
    let until = IAT + DAY;
    let card = license(EntitlementKind::Timed, Some(until));
    // 到期前 1 秒：离线可用，截止到 ent_until（比 exp 早）。
    let before = decide(Ok(&card), IAT, until - 1, Connectivity::Offline);
    assert_eq!(before.tier, Tier::Offline { until });
    // 到期当秒：锁定，提示续期。
    let at = decide(Ok(&card), IAT, until, Connectivity::Offline);
    assert_eq!(at.tier, Tier::Locked(LockReason::EntitlementExpired));
    let after = decide(Ok(&card), IAT, until + 1, Connectivity::Offline);
    assert_eq!(after.tier, Tier::Locked(LockReason::EntitlementExpired));
}

#[test]
fn offline_until_takes_earlier_of_exp_and_ent_until() {
    // 月卡比凭证晚到期：离线截止到 exp。
    let card = license(EntitlementKind::Timed, Some(EXP + 30 * DAY));
    assert_eq!(
        decide(Ok(&card), IAT, IAT, Connectivity::Offline).tier,
        Tier::Offline { until: EXP }
    );
    // 永久：截止到 exp。
    let lifetime = license(EntitlementKind::Lifetime, None);
    assert_eq!(
        decide(Ok(&lifetime), IAT, IAT, Connectivity::Offline).tier,
        Tier::Offline { until: EXP }
    );
}

#[test]
fn offline_without_entitlement_is_locked() {
    let none = license(EntitlementKind::None, None);
    assert_eq!(
        decide(Ok(&none), IAT, IAT, Connectivity::Offline).tier,
        Tier::Locked(LockReason::NoEntitlement)
    );
}

#[test]
fn expired_token_means_grace_expired() {
    // verify 在 now == exp 时返回 Expired；decide 把它换成「离线使用期已过」。
    assert_eq!(
        decide(Err(TokenError::Expired), IAT, EXP, Connectivity::Offline).tier,
        Tier::Locked(LockReason::GraceExpired)
    );
    for error in [
        TokenError::Malformed,
        TokenError::BadSignature,
        TokenError::BadPayload,
        TokenError::WrongDevice,
    ] {
        assert_eq!(
            decide(Err(error), IAT, IAT, Connectivity::Offline).tier,
            Tier::Locked(LockReason::LicenseInvalid),
            "{error:?}"
        );
    }
}

#[test]
fn offline_rollback_locks_and_keeps_high_water() {
    let card = license(EntitlementKind::Lifetime, None);
    let high = IAT + DAY;
    let access = decide(Ok(&card), high, IAT, Connectivity::Offline);
    assert_eq!(access.tier, Tier::Locked(LockReason::ClockRollback));
    // 倒退时不能把最晚时间往回改，否则再判一次就放行了。
    assert_eq!(access.high_water, high);
}

#[test]
fn renew_warning_three_days_before() {
    let until = IAT + 3 * DAY;
    let card = license(EntitlementKind::Timed, Some(until));
    assert!(decide(Ok(&card), IAT, IAT, Connectivity::Offline).renew_soon);
    let later = license(EntitlementKind::Timed, Some(until + 1));
    assert!(!decide(Ok(&later), IAT, IAT, Connectivity::Offline).renew_soon);
}

// ---------- 使用权：联网 ----------

#[test]
fn online_follows_server_entitlement() {
    // 本地凭证说没有使用权，但服务端刚兑换过：以服务端为准。
    let stale = license(EntitlementKind::None, None);
    let access = decide(
        Ok(&stale),
        IAT,
        IAT,
        online(EntitlementKind::Lifetime, true, None),
    );
    assert_eq!(access.tier, Tier::Online);
    assert!(!access.renew_soon);
}

#[test]
fn online_inactive_or_lapsed_is_expired() {
    let card = license(EntitlementKind::Timed, Some(IAT + DAY));
    let inactive = decide(
        Ok(&card),
        IAT,
        IAT,
        online(EntitlementKind::Timed, false, Some(IAT - 1)),
    );
    assert_eq!(inactive.tier, Tier::Locked(LockReason::EntitlementExpired));
    // 服务端上次说还有效，但两次刷新之间到了期：同样锁定（只收紧不放宽）。
    let until = IAT + 600;
    let lapsed = decide(
        Ok(&card),
        IAT,
        until,
        online(EntitlementKind::Timed, true, Some(until)),
    );
    assert_eq!(lapsed.tier, Tier::Locked(LockReason::EntitlementExpired));
    let before = decide(
        Ok(&card),
        IAT,
        until - 1,
        online(EntitlementKind::Timed, true, Some(until)),
    );
    assert_eq!(before.tier, Tier::Online);
    assert!(before.renew_soon);
}

#[test]
fn online_none_is_no_entitlement_and_resets_clock() {
    let card = license(EntitlementKind::None, None);
    // 时间被调到过未来：联网后最晚时间重置为现在。
    let access = decide(
        Ok(&card),
        IAT + DAY,
        IAT + 60,
        online(EntitlementKind::None, false, None),
    );
    assert_eq!(access.tier, Tier::Locked(LockReason::NoEntitlement));
    assert_eq!(access.high_water, IAT + 60);
}

#[test]
fn tier_permissions() {
    assert!(Tier::Online.allows_local() && Tier::Online.allows_paid());
    let offline = Tier::Offline { until: EXP };
    assert!(offline.allows_local() && !offline.allows_paid());
    let locked = Tier::Locked(LockReason::NoEntitlement);
    assert!(!locked.allows_local() && !locked.allows_paid());
}
