//! 报审包清单和处理配方的共用测试向量：逐条比对错误码和解析结果。
//!
//! 直接读 `cloud/tests/commerce/signed_vectors_v1.json`（服务端 `test_signed_vectors.py` 生成，
//! 签名用的是生产代码），不复制一份：两边规则一改不一致，这里立刻失败。

use dossier_license::envelope::Purpose;
use dossier_license::{manifest, recipe, verify, TokenError, TrustedKey};
use serde_json::{json, Value};

const VECTORS: &str = include_str!("../../../cloud/tests/commerce/signed_vectors_v1.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("测试向量是合法 JSON")
}

fn trusted(case: &Value) -> Vec<TrustedKey> {
    case["trusted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| TrustedKey::from_b64url(key.as_str().unwrap()).expect("向量里的公钥合法"))
        .collect()
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

#[test]
fn contexts_and_check_order_match() {
    let doc = vectors();
    assert_eq!(
        doc["contexts"]["license"].as_str().unwrap().as_bytes(),
        Purpose::License.context()
    );
    assert_eq!(
        doc["contexts"]["package-manifest"]
            .as_str()
            .unwrap()
            .as_bytes(),
        Purpose::PackageManifest.context()
    );
    assert_eq!(
        doc["contexts"]["recipe"].as_str().unwrap().as_bytes(),
        Purpose::Recipe.context()
    );
    assert_eq!(
        strings(&doc["manifest_check_order"]),
        ["malformed", "bad_signature", "bad_payload", "wrong_device"]
    );
    assert_eq!(
        strings(&doc["recipe_check_order"]),
        [
            "malformed",
            "bad_signature",
            "bad_payload",
            "wrong_device",
            "wrong_task",
            "expired"
        ]
    );
}

#[test]
fn every_manifest_vector_matches() {
    let doc = vectors();
    let cases = doc["manifest_cases"].as_array().unwrap();
    // 向量数一变（服务端加了新的），这里提醒同步看一眼。
    assert_eq!(
        cases.len(),
        52,
        "清单向量条数变了，请确认桌面端规则是否需要跟进"
    );

    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let result = manifest::verify(
            case["manifest"].as_str().unwrap().as_bytes(),
            case["signature"].as_str().unwrap().as_bytes(),
            &trusted(case),
            case["device_id"].as_str().unwrap(),
        );
        let actual = match result {
            Ok(parsed) => json!({
                "package_id": parsed.package_id,
                "placeholder": parsed.signature.placeholder,
                "files": parsed.files.len(),
                "references": parsed.references.len(),
            }),
            Err(error) => json!({ "error": error.code() }),
        };
        let expected = match case.get("error") {
            Some(error) => json!({ "error": error }),
            None => case["expect"].clone(),
        };
        if actual != expected {
            failures.push(format!("{name}: 期望 {expected}，实际 {actual}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_recipe_vector_matches() {
    let doc = vectors();
    let cases = doc["recipe_cases"].as_array().unwrap();
    assert_eq!(
        cases.len(),
        53,
        "配方向量条数变了，请确认桌面端规则是否需要跟进"
    );

    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let result = recipe::verify(
            case["token"].as_str().unwrap(),
            &trusted(case),
            case["device_id"].as_str().unwrap(),
            case["task"].as_str().unwrap(),
            case["now"].as_u64().unwrap(),
        );
        let actual = match result {
            Ok(parsed) => json!({
                "kid": parsed.kid,
                "rid": parsed.recipe_id,
                "sub": parsed.user_id,
                "dev": parsed.device_id,
                "task": parsed.task,
                "platform": parsed.platform,
                "strength": parsed.strength,
                "seed": parsed.seed,
                "params": parsed.params,
                "iat": parsed.issued_at,
                "exp": parsed.expires_at,
            }),
            Err(error) => json!({ "error": error.code() }),
        };
        let expected = match case.get("error") {
            Some(error) => json!({ "error": error }),
            None => case["expect"].clone(),
        };
        if actual != expected {
            failures.push(format!("{name}: 期望 {expected}，实际 {actual}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// 配方和授权凭证的格式一样（`载荷.签名`），只靠用途前缀区分：拿配方去当授权凭证必须验不过。
#[test]
fn recipe_tokens_are_not_licenses() {
    let doc = vectors();
    let valid: Vec<&Value> = doc["recipe_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case.get("expect").is_some())
        .collect();
    assert!(!valid.is_empty());
    for case in valid {
        let result = verify(
            case["token"].as_str().unwrap(),
            &trusted(case),
            case["device_id"].as_str().unwrap(),
            case["now"].as_u64().unwrap(),
        );
        assert_eq!(
            result.err(),
            Some(TokenError::BadSignature),
            "{}",
            case["name"]
        );
    }
}

/// 清单签名不能拿去当配方签名：把清单字节和它的签名拼成配方的写法，必须验不过。
#[test]
fn manifest_signatures_are_not_recipes() {
    let doc = vectors();
    for case in doc["manifest_cases"].as_array().unwrap() {
        if case.get("expect").is_none() {
            continue;
        }
        let signature: Value = serde_json::from_str(case["signature"].as_str().unwrap()).unwrap();
        let payload = base64_url(case["manifest"].as_str().unwrap().as_bytes());
        let token = format!("{payload}.{}", signature["sig"].as_str().unwrap());
        let result = recipe::verify(&token, &trusted(case), "a".repeat(64).as_str(), "c", 0);
        assert_eq!(
            result.err(),
            Some(recipe::RecipeError::BadSignature),
            "{}",
            case["name"]
        );
    }
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
