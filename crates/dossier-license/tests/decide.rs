//! 判定规则的场景测试：用确定性测试私钥现场签凭证。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dossier_license::{decide, parse_key_list, LockReason, Plan, Verdict, SIGNING_CONTEXT};
use ed25519_dalek::{Signer, SigningKey};

const M1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const M2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const IAT: u64 = 1_800_000_000;
const DAY: u64 = 86_400;

fn seed() -> SigningKey {
    let bytes: [u8; 32] = core::array::from_fn(|i| i as u8);
    SigningKey::from_bytes(&bytes)
}

fn kid(sk: &SigningKey) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(sk.verifying_key().as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn token(sk: &SigningKey, plan: &str, until: Option<u64>, exp: u64) -> String {
    let until = until.map_or("null".to_string(), |u| u.to_string());
    let payload = format!(
        r#"{{"exp":{exp},"iat":{IAT},"kid":"{}","lid":1,"mid":"{M1}","plan":"{plan}","until":{until},"v":1}}"#,
        kid(sk)
    );
    let mut msg = SIGNING_CONTEXT.to_vec();
    msg.extend_from_slice(payload.as_bytes());
    let sig = sk.sign(&msg);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

fn keys() -> Vec<dossier_license::TrustedKey> {
    let pk = URL_SAFE_NO_PAD.encode(seed().verifying_key().as_bytes());
    parse_key_list(&[pk.as_str()]).unwrap()
}

#[test]
fn active_month() {
    let t = token(&seed(), "month", Some(IAT + 30 * DAY), IAT + 7 * DAY);
    match decide(&t, M1, IAT + DAY, 0, Ok(&keys())) {
        Verdict::Active(a) => {
            assert_eq!(a.plan, Plan::Month);
            assert_eq!(a.exp, IAT + 7 * DAY);
        }
        v => panic!("{v:?}"),
    }
}

#[test]
fn copied_to_other_machine_is_locked() {
    let t = token(&seed(), "year", Some(IAT + 365 * DAY), IAT + 7 * DAY);
    assert_eq!(
        decide(&t, M2, IAT + DAY, 0, Ok(&keys())),
        Verdict::Locked(LockReason::WrongMachine)
    );
}

#[test]
fn offline_grace_ends_at_exp_even_for_lifetime() {
    let t = token(&seed(), "lifetime", None, IAT + 7 * DAY);
    assert!(decide(&t, M1, IAT + 6 * DAY, 0, Ok(&keys())).is_active());
    assert_eq!(
        decide(&t, M1, IAT + 7 * DAY, 0, Ok(&keys())),
        Verdict::Locked(LockReason::Expired)
    );
}

#[test]
fn turning_clock_back_after_use_is_locked() {
    let t = token(&seed(), "month", Some(IAT + 30 * DAY), IAT + 7 * DAY);
    // 用到第 6 天后把时间拨回第 1 天，想多用几天
    assert_eq!(
        decide(&t, M1, IAT + DAY, IAT + 6 * DAY, Ok(&keys())),
        Verdict::Locked(LockReason::ClockRollback)
    );
}

#[test]
fn tampered_payload_field_is_rejected() {
    let t = token(&seed(), "trial", Some(IAT + 3 * DAY), IAT + 3 * DAY);
    let (p, s) = t.split_once('.').unwrap();
    let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(p).unwrap())
        .unwrap()
        .replace("trial", "year ");
    let forged = format!("{}.{s}", URL_SAFE_NO_PAD.encode(payload));
    assert_eq!(
        decide(&forged, M1, IAT + DAY, 0, Ok(&keys())),
        Verdict::Locked(LockReason::BadSignature)
    );
}

#[test]
fn token_signed_by_stranger_is_rejected() {
    let stranger = SigningKey::from_bytes(&[7u8; 32]);
    let t = token(&stranger, "lifetime", None, IAT + 7 * DAY);
    assert_eq!(
        decide(&t, M1, IAT + DAY, 0, Ok(&keys())),
        Verdict::Locked(LockReason::BadSignature)
    );
}

#[test]
fn every_lock_reason_has_user_message() {
    use LockReason::*;
    for r in [
        NotActivated,
        NotConfigured,
        MachineUnavailable,
        Malformed,
        BadSignature,
        BadPayload,
        WrongMachine,
        ClockRollback,
        Expired,
        Tampered,
        Revoked,
    ] {
        assert!(!r.message().is_empty() && !r.code().is_empty());
    }
}
