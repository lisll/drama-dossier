//! 服务端和桌面端共用的授权凭证测试向量：逐条比对错误码和解析结果。
//!
//! 直接读 `cloud/tests/commerce/license_vectors_v1.json`，不复制一份：
//! 服务端的参考实现（`license_token.verify`）跑的也是这一份，两边规则一改不一致，这里立刻失败。

use dossier_license::{verify, EntitlementKind, TrustedKey};
use serde_json::Value;

const VECTORS: &str = include_str!("../../../cloud/tests/commerce/license_vectors_v1.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("测试向量是合法 JSON")
}

#[test]
fn check_order_matches_desktop_error_codes() {
    let doc = vectors();
    let order: Vec<&str> = doc["check_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "malformed",
            "bad_signature",
            "bad_payload",
            "wrong_device",
            "expired"
        ]
    );
    assert_eq!(doc["signing_context"], "drama-dossier/license/v1\n");
}

#[test]
fn every_vector_matches() {
    let doc = vectors();
    let cases = doc["cases"].as_array().unwrap();
    // 向量数一变（服务端加了新的），这里提醒同步看一眼。
    assert_eq!(
        cases.len(),
        53,
        "测试向量条数变了，请确认桌面端规则是否需要跟进"
    );

    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let trusted: Vec<TrustedKey> = case["trusted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| TrustedKey::from_b64url(key.as_str().unwrap()).expect("向量里的公钥合法"))
            .collect();
        let result = verify(
            case["token"].as_str().unwrap(),
            &trusted,
            case["device_id"].as_str().unwrap(),
            case["now"].as_u64().unwrap(),
        );
        match (case.get("error").and_then(Value::as_str), result) {
            (Some(expected), Err(error)) if error.code() == expected => {}
            (Some(expected), other) => {
                failures.push(format!("{name}：期望 {expected}，得到 {other:?}"))
            }
            (None, Err(error)) => failures.push(format!("{name}：期望通过，得到 {}", error.code())),
            (None, Ok(license)) => {
                let expect = &case["expect"];
                let entitlement = EntitlementKind::parse(expect["entitlement"].as_str().unwrap());
                let checks = [
                    ("kid", license.kid == expect["kid"].as_str().unwrap()),
                    (
                        "user_id",
                        license.user_id == expect["user_id"].as_str().unwrap(),
                    ),
                    (
                        "device_id",
                        license.device_id == expect["device_id"].as_str().unwrap(),
                    ),
                    ("entitlement", Some(license.entitlement) == entitlement),
                    (
                        "entitlement_until",
                        license.entitlement_until == expect["entitlement_until"].as_u64(),
                    ),
                    (
                        "issued_at",
                        Some(license.issued_at) == expect["issued_at"].as_u64(),
                    ),
                    (
                        "expires_at",
                        Some(license.expires_at) == expect["expires_at"].as_u64(),
                    ),
                ];
                for (field, ok) in checks {
                    if !ok {
                        failures.push(format!("{name}：字段 {field} 不一致：{license:?}"));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "不一致的向量：\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_error_code_is_covered() {
    let doc = vectors();
    let mut seen: Vec<&str> = doc["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case.get("error").and_then(Value::as_str))
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen,
        [
            "bad_payload",
            "bad_signature",
            "expired",
            "malformed",
            "wrong_device"
        ]
    );
}
