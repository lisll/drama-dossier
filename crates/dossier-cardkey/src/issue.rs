//! 本机签发凭证（`test-issuer` 开关）。
//!
//! 与服务端 `license-server/src/dd_license/token.py::sign` 输出逐字节相同：
//! 键按字母序、紧凑、纯 ASCII 的 JSON，签名对象 = [`SIGNING_CONTEXT`] + payload。
//! 只给离线测试版与单元测试用；正式版不开启这个开关，二进制里没有签发代码。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

use crate::keys::key_id;
use crate::token::{Plan, SIGNING_CONTEXT};

/// 由 32 字节种子得到签名私钥。
pub fn signing_key(seed: [u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&seed)
}

/// 私钥对应的公钥（base64url，无填充），可直接交给 [`crate::parse_key_list`]。
pub fn public_key_b64(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

/// 用给定私钥签发一份凭证。`mid` 必须是 64 位小写十六进制（由调用方保证）。
pub fn issue_token(
    key: &SigningKey,
    lid: u64,
    mid: &str,
    plan: Plan,
    until: Option<u64>,
    iat: u64,
    exp: u64,
) -> String {
    let kid = key_id(key.verifying_key().as_bytes());
    let until = until.map_or_else(|| "null".to_string(), |u| u.to_string());
    // 字段按字母序：exp iat kid lid mid plan until v
    let payload = format!(
        r#"{{"exp":{exp},"iat":{iat},"kid":"{kid}","lid":{lid},"mid":"{mid}","plan":"{}","until":{until},"v":1}}"#,
        plan.as_str()
    );
    let mut msg = Vec::with_capacity(SIGNING_CONTEXT.len() + payload.len());
    msg.extend_from_slice(SIGNING_CONTEXT);
    msg.extend_from_slice(payload.as_bytes());
    let sig = key.sign(&msg);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload.as_bytes()),
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decide, parse_key_list, Verdict};

    const MID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn issued_token_passes_decide() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let pk = URL_SAFE_NO_PAD.encode(sk.verifying_key().as_bytes());
        let keys = parse_key_list(&[pk.as_str()]).unwrap();
        let t = issue_token(&sk, 7, MID, Plan::Trial, Some(2000), 1000, 2000);
        match decide(&t, MID, 1500, 0, Ok(&keys)) {
            Verdict::Active(a) => {
                assert_eq!(a.plan, Plan::Trial);
                assert_eq!((a.iat, a.exp, a.until), (1000, 2000, Some(2000)));
            }
            v => panic!("{v:?}"),
        }
    }

    /// 与服务端 sign() 的输出逐字节一致：用向量文件里的种子 0..32 与固定字段比对。
    #[test]
    fn matches_server_canonical_form() {
        let sk = SigningKey::from_bytes(&core::array::from_fn(|i| i as u8));
        let t = issue_token(
            &sk,
            1,
            MID,
            Plan::Lifetime,
            None,
            1_800_000_000,
            1_800_003_600,
        );
        let (p, _) = t.split_once('.').unwrap();
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap();
        assert_eq!(
            payload,
            format!(
                r#"{{"exp":1800003600,"iat":1800000000,"kid":"56475aa75463474c","lid":1,"mid":"{MID}","plan":"lifetime","until":null,"v":1}}"#
            )
        );
    }
}
