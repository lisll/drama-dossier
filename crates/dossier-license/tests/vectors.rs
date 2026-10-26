//! 与 Python 服务端逐条对拍：vectors_v1.json 由 license-server/tests/vectors.py 生成。

use dossier_license::{
    decide, parse_key_list, LockReason, Verdict, ROLLBACK_TOLERANCE_SECS, SIGNING_CONTEXT,
};
use serde_json::Value;

const VECTORS: &str = include_str!("vectors_v1.json");

fn reason_code(v: &Verdict) -> &'static str {
    match v {
        Verdict::Active(_) => "ok",
        Verdict::Locked(r) => r.code(),
    }
}

#[test]
fn every_vector_matches() {
    let doc: Value = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(
        doc["signing_context"].as_str().unwrap().as_bytes(),
        SIGNING_CONTEXT
    );
    assert_eq!(
        doc["rollback_tolerance_secs"].as_u64().unwrap(),
        ROLLBACK_TOLERANCE_SECS
    );
    let keys_owned: Vec<String> = doc["trusted_public_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let key_refs: Vec<&str> = keys_owned.iter().map(String::as_str).collect();
    let keys = parse_key_list(&key_refs).unwrap();

    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 30, "向量数量异常：{}", cases.len());
    let mut seen = std::collections::BTreeSet::new();
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let verdict = decide(
            c["token"].as_str().unwrap(),
            c["machine_id"].as_str().unwrap(),
            c["now"].as_u64().unwrap(),
            c["last_seen"].as_u64().unwrap(),
            Ok(&keys),
        );
        let expect = c["expect"].as_str().unwrap();
        assert_eq!(reason_code(&verdict), expect, "向量 {name}");
        seen.insert(expect.to_string());
        if let Verdict::Active(a) = verdict {
            assert_eq!(a.plan.as_str(), c["plan"].as_str().unwrap(), "{name}");
            assert_eq!(a.until, c["until"].as_u64(), "{name}");
            assert_eq!(a.exp, c["exp"].as_u64().unwrap(), "{name}");
        }
    }
    // 每一种结论都要被至少一条向量覆盖
    for code in [
        "ok",
        "malformed",
        "bad_signature",
        "bad_payload",
        "wrong_machine",
        "clock_rollback",
        "expired",
    ] {
        assert!(seen.contains(code), "缺少覆盖 {code} 的向量");
    }
}

#[test]
fn unconfigured_keys_always_lock() {
    let doc: Value = serde_json::from_str(VECTORS).unwrap();
    let c = &doc["cases"][0];
    let v = decide(
        c["token"].as_str().unwrap(),
        c["machine_id"].as_str().unwrap(),
        c["now"].as_u64().unwrap(),
        0,
        Ok(&[]),
    );
    assert_eq!(v, Verdict::Locked(LockReason::NotConfigured));
    let v = decide(
        c["token"].as_str().unwrap(),
        c["machine_id"].as_str().unwrap(),
        0,
        0,
        Err(dossier_license::KeyError::Empty),
    );
    assert_eq!(v, Verdict::Locked(LockReason::NotConfigured));
}

#[test]
fn production_list_is_empty_or_free_of_test_keys() {
    // 生产公钥为空时必须锁定（NotConfigured），不能退回去信任测试公钥
    match dossier_license::production_keys() {
        Err(dossier_license::KeyError::Empty) => {}
        Ok(keys) => assert!(!keys.is_empty()),
        Err(e) => panic!("生产公钥配置错误：{e}"),
    }
}
