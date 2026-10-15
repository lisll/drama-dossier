//! 比服务端参考实现更严的几处、公钥列表的配置检查。
//!
//! 这里自己签凭证：种子是写死的测试值，和测试向量的两个公开种子都不同。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dossier_license::keys::{key_id, parse_key_list};
use dossier_license::{verify, EntitlementKind, KeyError, TokenError, TrustedKey};
use ed25519_dalek::{Signer, SigningKey};

const CONTEXT: &[u8] = b"drama-dossier/license/v1\n";
const DEVICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NOW: u64 = 1_767_225_600;

/// 测试向量里的两把公开公钥（种子 0..32、32..64）。
const TEST_KEY_A: &str = "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg";
const TEST_KEY_B: &str = "Kay64UG8yvCyLhqU000LxzYeUm0L_hLIl5S8kyKWbdc";

fn signing_key(fill: u8) -> SigningKey {
    SigningKey::from_bytes(&[fill; 32])
}

fn public_b64(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

fn trusted(key: &SigningKey) -> TrustedKey {
    TrustedKey::from_bytes(&key.verifying_key().to_bytes()).unwrap()
}

fn kid(key: &SigningKey) -> String {
    key_id(&key.verifying_key().to_bytes())
}

/// 用给定私钥签一段原始载荷字节。
fn sign(key: &SigningKey, payload: &str) -> String {
    let mut message = CONTEXT.to_vec();
    message.extend_from_slice(payload.as_bytes());
    let signature = key.sign(&message);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload.as_bytes()),
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

/// 一段合法的载荷，`extra` 原样拼在对象末尾（用来塞要测的字段）。
fn payload(key: &SigningKey, iat: &str, extra: &str) -> String {
    format!(
        r#"{{"v":1,"kid":"{}","sub":"u-1","dev":"{DEVICE}","ent":"lifetime","ent_until":null,"iat":{iat},"exp":{}{extra}}}"#,
        kid(key),
        NOW + 3600
    )
}

#[test]
fn own_signature_round_trip() {
    let key = signing_key(7);
    let token = sign(&key, &payload(&key, &NOW.to_string(), ""));
    let license = verify(&token, &[trusted(&key)], DEVICE, NOW).unwrap();
    assert_eq!(license.entitlement, EntitlementKind::Lifetime);
    assert_eq!(license.kid, kid(&key));
}

#[test]
fn negative_zero_integer_is_rejected() {
    // 参考实现（Python json）把 -0 读成 0 并接受；这里拒绝。服务端从来不输出 -0。
    let key = signing_key(7);
    let token = sign(&key, &payload(&key, "-0", ""));
    assert_eq!(
        verify(&token, &[trusted(&key)], DEVICE, NOW),
        Err(TokenError::BadPayload)
    );
}

#[test]
fn lone_surrogate_is_rejected() {
    // 参考实现接受单个 UTF-16 代理字符；这里拒绝。
    let key = signing_key(7);
    let token = sign(
        &key,
        &payload(&key, &NOW.to_string(), r#","note":"\ud800""#),
    );
    assert_eq!(
        verify(&token, &[trusted(&key)], DEVICE, NOW),
        Err(TokenError::BadPayload)
    );
}

#[test]
fn deep_nesting_is_rejected() {
    // 嵌套超过 128 层（serde_json 的上限）：参考实现接受，这里拒绝。
    let key = signing_key(7);
    let deep = format!(r#","x":{}{}"#, "[".repeat(200), "]".repeat(200));
    let token = sign(&key, &payload(&key, &NOW.to_string(), &deep));
    assert_eq!(
        verify(&token, &[trusted(&key)], DEVICE, NOW),
        Err(TokenError::BadPayload)
    );
}

#[test]
fn second_key_in_list_is_accepted() {
    // 密钥轮换：受信列表里第二把公钥签的凭证同样通过，kid 是实际验过签的那把。
    let old = signing_key(7);
    let new = signing_key(9);
    let token = sign(&new, &payload(&new, &NOW.to_string(), ""));
    let license = verify(&token, &[trusted(&old), trusted(&new)], DEVICE, NOW).unwrap();
    assert_eq!(license.kid, kid(&new));
}

#[test]
fn kid_of_other_trusted_key_is_rejected() {
    // 用新钥签名、载荷里却写旧钥的 kid：签名能过，但 kid 和实际签名的公钥对不上。
    let old = signing_key(7);
    let new = signing_key(9);
    let forged = payload(&new, &NOW.to_string(), "").replace(&kid(&new), &kid(&old));
    let token = sign(&new, &forged);
    assert_eq!(
        verify(&token, &[trusted(&old), trusted(&new)], DEVICE, NOW),
        Err(TokenError::BadPayload)
    );
}

#[test]
fn key_list_rejects_test_vector_keys() {
    assert_eq!(
        parse_key_list([TEST_KEY_A]).unwrap_err(),
        KeyError::TestVector(1)
    );
    let real = public_b64(&signing_key(7));
    assert_eq!(
        parse_key_list([real.as_str(), TEST_KEY_B]).unwrap_err(),
        KeyError::TestVector(2)
    );
}

#[test]
fn key_list_rejects_empty_and_malformed() {
    assert_eq!(
        parse_key_list(std::iter::empty::<&str>()).unwrap_err(),
        KeyError::Empty
    );
    // 只有空白和空项：等于没配置。
    assert_eq!(parse_key_list(["", "  "]).unwrap_err(), KeyError::Empty);
    assert_eq!(
        parse_key_list(["not-a-key"]).unwrap_err(),
        KeyError::Invalid(1)
    );
    // 带填充的写法不认。
    let padded = format!("{}=", public_b64(&signing_key(7)));
    assert_eq!(
        parse_key_list([padded.as_str()]).unwrap_err(),
        KeyError::Invalid(1)
    );
    // 长度不是 32 字节。
    let short = URL_SAFE_NO_PAD.encode([1u8; 16]);
    assert_eq!(
        parse_key_list([short.as_str()]).unwrap_err(),
        KeyError::Invalid(1)
    );
}

#[test]
fn key_list_trims_and_keeps_order() {
    let a = public_b64(&signing_key(7));
    let b = public_b64(&signing_key(9));
    let spaced_a = format!("  {a} ");
    // 逗号分隔时末尾多一个逗号会产生空项，应当忽略。
    let keys = parse_key_list([spaced_a.as_str(), "", b.as_str()]).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].kid(), kid(&signing_key(7)));
    assert_eq!(keys[1].kid(), kid(&signing_key(9)));
}
